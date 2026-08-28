use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use anyhow::Result;
#[cfg(test)]
use bookforge_core::NullProgressSink;
use bookforge_core::{ProgressEvent, ProgressSink, progress::now_ms, segment::SegmentStatus};
use bookforge_llm::SegmentTranslation;
use bookforge_store::{JobStore, SaveNeedsReview, SaveTranslation};
use tokio::{sync::mpsc, task::JoinHandle};

pub const CHECKPOINT_QUEUE_CAPACITY: usize = 64;

pub enum CheckpointCommand {
    SaveTranslation {
        job_id: String,
        translation: Box<SegmentTranslation>,
        provider: String,
        model: String,
        prompt_version: String,
    },
}

impl CheckpointCommand {
    fn segment_id_for_progress(&self) -> Option<String> {
        let CheckpointCommand::SaveTranslation { translation, .. } = self;
        Some(translation.segment_id.0.clone())
    }

    fn segment_finished_event(&self) -> Option<ProgressEvent> {
        let CheckpointCommand::SaveTranslation { translation, .. } = self;
        Some(ProgressEvent::SegmentFinished {
            segment_id: translation.segment_id.0.clone(),
            status: segment_status_str(translation.status).to_string(),
            input_tokens: translation.input_tokens,
            output_tokens: translation.output_tokens,
            timestamp_ms: now_ms(),
        })
    }
}

#[derive(Clone)]
pub struct CheckpointSender {
    pub tx: mpsc::Sender<CheckpointCommand>,
    #[allow(dead_code)]
    queue_depth: Arc<AtomicUsize>,
    #[allow(dead_code)]
    progress: Arc<dyn ProgressSink>,
}

impl CheckpointSender {
    #[allow(dead_code)]
    pub async fn send(
        &self,
        cmd: CheckpointCommand,
    ) -> std::result::Result<(), bookforge_llm::LlmError> {
        let queued = self.queue_depth.fetch_add(1, Ordering::AcqRel) + 1;

        match self.tx.send(cmd).await {
            Ok(()) => {
                self.progress.emit(ProgressEvent::CheckpointQueued {
                    queued,
                    timestamp_ms: now_ms(),
                });
                Ok(())
            }
            Err(_) => {
                self.queue_depth.fetch_sub(1, Ordering::AcqRel);
                Err(bookforge_llm::LlmError::Provider(
                    "checkpoint queue closed; checkpoint writer may have failed".to_string(),
                ))
            }
        }
    }
}

fn segment_status_str(status: SegmentStatus) -> &'static str {
    match status {
        SegmentStatus::Queued => "queued",
        SegmentStatus::Succeeded => "succeeded",
        SegmentStatus::Failed => "failed",
        SegmentStatus::RetryPending => "retry_pending",
        SegmentStatus::NeedsReview => "needs_review",
        SegmentStatus::SkippedCached => "skipped_cached",
    }
}

pub struct CheckpointWriter {
    tx: mpsc::Sender<CheckpointCommand>,
    join: JoinHandle<Result<()>>,
    queue_depth: Arc<AtomicUsize>,
    progress: Arc<dyn ProgressSink>,
}

impl CheckpointWriter {
    pub fn spawn(db_path: PathBuf, progress: Arc<dyn ProgressSink>) -> Self {
        let (tx, mut rx) = mpsc::channel::<CheckpointCommand>(CHECKPOINT_QUEUE_CAPACITY);
        let queue_depth = Arc::new(AtomicUsize::new(0));

        let writer_depth = queue_depth.clone();
        let writer_progress = progress.clone();

        let join = tokio::task::spawn_blocking(move || -> Result<()> {
            let store = JobStore::open(&db_path)
                .map_err(|err| anyhow::anyhow!("checkpoint writer open failed: {err}"))?;
            // Canonical open point: drain warn-on-open storage diagnostics so
            // legacy unknown statuses / skipped hardening surface in logs
            // instead of rotting unnoticed on the writer's connection.
            for diagnostic in store.take_diagnostics() {
                tracing::warn!(surface = "checkpoint_writer", "{diagnostic}");
            }

            let mut flushed = 0usize;
            let mut dropped = 0usize;

            while let Some(cmd) = rx.blocking_recv() {
                writer_depth.fetch_sub(1, Ordering::AcqRel);

                let segment_id = cmd.segment_id_for_progress();
                let segment_finished = cmd.segment_finished_event();
                let started = std::time::Instant::now();

                // One poisoned command (for example an FK violation from a
                // phantom segment id) must not kill the writer or poison
                // every later checkpoint: log-and-continue so the rest of a
                // long paid run still persists honestly.
                if let Err(error) = apply(&store, cmd) {
                    dropped += 1;
                    let message = format!("checkpoint write failed: {error}");
                    writer_progress.emit(ProgressEvent::Error {
                        kind: "checkpoint_write".to_string(),
                        message: message.clone(),
                        timestamp_ms: now_ms(),
                    });
                    tracing::warn!(
                        segment_id = segment_id.as_deref().unwrap_or("?"),
                        dropped_count = dropped,
                        "{message}; keeping the checkpoint writer alive"
                    );
                    continue;
                }

                if let Some(event) = segment_finished {
                    writer_progress.emit(event);
                }
                flushed += 1;
                writer_progress.emit(ProgressEvent::CheckpointFlushed {
                    segment_id,
                    flushed_count: flushed,
                    latency_ms: Some(started.elapsed().as_millis() as u64),
                    timestamp_ms: now_ms(),
                });
            }

            if dropped > 0 {
                // Never hide how much work was lost: surface the final tally
                // alongside the flush count so operators can quantify damage.
                let message = format!(
                    "checkpoint writer dropped {dropped} command(s) after write errors; \
                     {flushed} checkpoint(s) were persisted"
                );
                tracing::warn!(dropped, flushed, "{message}");
                writer_progress.emit(ProgressEvent::Warning {
                    kind: "checkpoint_dropped_commands".to_string(),
                    message,
                    timestamp_ms: now_ms(),
                });
            }

            Ok(())
        });

        Self {
            tx,
            join,
            queue_depth,
            progress,
        }
    }

    pub fn sender(&self) -> CheckpointSender {
        CheckpointSender {
            tx: self.tx.clone(),
            queue_depth: self.queue_depth.clone(),
            progress: self.progress.clone(),
        }
    }

    pub async fn shutdown(self) -> Result<()> {
        let CheckpointWriter { tx, join, .. } = self;
        drop(tx);
        let writer_result = join
            .await
            .map_err(|err| anyhow::anyhow!("checkpoint writer task join failed: {err}"))?;
        writer_result.map_err(|err| anyhow::anyhow!("checkpoint writer failed: {err}"))
    }
}

/// Engine-provided structured findings for the checkpoint paths.
///
/// The engine workstream landed `BatchItemFailure.findings` (serde-default
/// tolerant); until it also surfaces the structured findings on
/// `SegmentTranslation` — the value the checkpoint writer receives — the CLI
/// persists the legacy-parsed fallback below. When the field lands this
/// placeholder becomes `&translation.findings` at the two call sites and the
/// `findings_for_checkpoint` precedence turns it on with no other change.
const ENGINE_FINDINGS_PLACEHOLDER: &[bookforge_core::finding::EngineFinding] = &[];

fn apply(store: &JobStore, cmd: CheckpointCommand) -> Result<()> {
    let CheckpointCommand::SaveTranslation {
        job_id,
        translation,
        provider,
        model,
        prompt_version,
    } = cmd;
    let joined = translation.joined_text();
    match translation.status {
        SegmentStatus::Succeeded => {
            store.save_translation_with_findings(
                SaveTranslation {
                    job_id: &job_id,
                    segment_id: &translation.segment_id.0,
                    translated_text: &joined,
                    blocks: &translation.blocks,
                    provider: &provider,
                    model: &model,
                    prompt_version: &prompt_version,
                    input_tokens: translation.input_tokens,
                    input_cached_tokens: translation.input_cached_tokens,
                    output_tokens: translation.output_tokens,
                    tokens_estimated: translation.tokens_estimated,
                },
                translation.error.as_deref(),
            )?;
        }
        SegmentStatus::NeedsReview => {
            store.save_needs_review(SaveNeedsReview {
                job_id: &job_id,
                segment_id: &translation.segment_id.0,
                preserved_text: &joined,
                blocks: &translation.blocks,
                provider: &provider,
                model: &model,
                prompt_version: &prompt_version,
                error: translation
                    .error
                    .as_deref()
                    .unwrap_or("translation requires review"),
                input_tokens: translation.input_tokens,
                input_cached_tokens: translation.input_cached_tokens,
                output_tokens: translation.output_tokens,
                tokens_estimated: translation.tokens_estimated,
            })?;
            persist_checkpoint_findings(
                store,
                &job_id,
                &translation.segment_id.0,
                ENGINE_FINDINGS_PLACEHOLDER,
                translation.error.as_deref(),
            );
        }
        SegmentStatus::Failed => {
            store.mark_segment_failed_if_unfinished(
                &job_id,
                &translation.segment_id.0,
                translation.error.as_deref().unwrap_or("translation failed"),
            )?;
            if store.segment_records(&job_id).is_ok_and(|records| {
                records.iter().any(|record| {
                    record.id == translation.segment_id.0 && record.status == "failed"
                })
            }) {
                persist_checkpoint_findings(
                    store,
                    &job_id,
                    &translation.segment_id.0,
                    ENGINE_FINDINGS_PLACEHOLDER,
                    translation.error.as_deref(),
                );
            }
        }
        _ => {}
    }
    Ok(())
}

/// Persist the structured QA findings for a needs-review/failed checkpoint.
///
/// Structured engine findings win when the engine provided them
/// (`BatchItemFailure.findings` — serde-default tolerant). Otherwise the
/// legacy concatenated error string is decomposed through the shared core
/// parser (`findings_from_legacy_error_text`) so old-style errors still land
/// in the block finding vocabulary with honest per-instance severity instead
/// of a CLI-local re-parse. The legacy error string itself keeps flowing into
/// `segments.error` as before — other surfaces still read it. Findings are
/// instrumentation: a failed findings write must never fail the surrounding
/// checkpoint.
fn persist_checkpoint_findings(
    store: &JobStore,
    job_id: &str,
    segment_id: &str,
    engine_findings: &[bookforge_core::finding::EngineFinding],
    legacy_error: Option<&str>,
) {
    let findings = findings_for_checkpoint(engine_findings, legacy_error);
    if findings.is_empty() {
        return;
    }
    if let Err(error) = store.record_segment_engine_findings(job_id, segment_id, &findings) {
        tracing::warn!(
            job_id,
            segment_id,
            "structured findings write failed: {error}; keeping the checkpoint"
        );
    }
}

/// Structured findings for one checkpoint. Non-empty engine findings win;
/// otherwise the legacy error string decomposes through the core parser
/// (empty/absent errors produce no findings).
fn findings_for_checkpoint(
    engine_findings: &[bookforge_core::finding::EngineFinding],
    legacy_error: Option<&str>,
) -> Vec<bookforge_core::finding::EngineFinding> {
    if !engine_findings.is_empty() {
        return engine_findings.to_vec();
    }
    legacy_error
        .map(bookforge_core::finding::findings_from_legacy_error_text)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookforge_core::{
        ir::BlockId,
        segment::{
            BlockTranslation, Segment, SegmentBlock, SegmentConstraints, SegmentContext, SegmentId,
            SegmentMetadata, SegmentSource, SegmentTextRun,
        },
    };
    use bookforge_store::CreateJob;
    use std::{fs, sync::Mutex, time::SystemTime};

    #[derive(Default)]
    struct RecordingProgressSink {
        events: Mutex<Vec<ProgressEvent>>,
    }

    impl RecordingProgressSink {
        fn events(&self) -> Vec<ProgressEvent> {
            self.events.lock().expect("events mutex").clone()
        }
    }

    impl ProgressSink for RecordingProgressSink {
        fn emit(&self, event: ProgressEvent) {
            self.events.lock().expect("events mutex").push(event);
        }
    }

    fn test_translation(
        segment_id: &str,
        ordinal: usize,
        status: SegmentStatus,
    ) -> SegmentTranslation {
        SegmentTranslation {
            segment_id: SegmentId(segment_id.to_string()),
            ordinal,
            block_ids: vec![BlockId("b_000000".to_string())],
            blocks: vec![BlockTranslation {
                block_id: BlockId("b_000000".to_string()),
                text: "Translated text".to_string(),
            }],
            checksum: "checksum".to_string(),
            status,
            template: "test".to_string(),
            error: if matches!(status, SegmentStatus::Failed) {
                Some("simulated failure".to_string())
            } else {
                None
            },
            input_tokens: Some(10),
            input_cached_tokens: Some(0),
            output_tokens: Some(5),
            tokens_estimated: false,
        }
    }

    fn test_segment(id: &str, ordinal: usize) -> Segment {
        Segment {
            id: SegmentId(id.to_string()),
            section_id: bookforge_core::ir::SectionId("sec_000000".to_string()),
            ordinal,
            block_ids: vec![BlockId("b_000000".to_string())],
            source: SegmentSource {
                text: format!("Source {ordinal}"),
                blocks: vec![SegmentBlock {
                    block_id: BlockId("b_000000".to_string()),
                    kind: "paragraph".to_string(),
                    text: format!("Source {ordinal}"),
                    text_runs: vec![SegmentTextRun {
                        id: format!("r{ordinal}"),
                        text: format!("Source {ordinal}"),
                    }],
                    protected_spans: Vec::new(),
                }],
                token_estimate: 2,
            },
            context: SegmentContext::default(),
            metadata: SegmentMetadata::default(),
            constraints: SegmentConstraints::default(),
            checksum: format!("checksum_{ordinal}"),
        }
    }

    fn temp_path(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        // Counter disambiguates calls that share a timestamp (the clock is
        // coarse on some platforms), keeping parallel tests off each other's
        // temp files.
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "bookforge-chk-test-{}-{nanos}-{seq}-{name}",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn checkpoint_writer_flushes_all_translations_before_shutdown() {
        let db_path = temp_path("flush.sqlite");
        let input_path = temp_path("input.epub");

        let store = JobStore::open(&db_path).expect("store open for setup");
        fs::write(&input_path, b"epub bytes").expect("input fixture writable");
        let job = store
            .create_job(CreateJob {
                input: &input_path,
                output: &temp_path("output.epub"),
                source_lang: Some("English"),
                target_lang: "Italian",
                provider: "mock",
                model: "mock-model",
                base_url: None,
                api_key_env: None,
                book_id: None,
                series_id: None,
            })
            .expect("job created");
        store
            .insert_segments(
                &job.id,
                &[test_segment("seg_a", 0), test_segment("seg_b", 1)],
                "v1",
                "mock",
                "mock-model",
                "test_ns",
            )
            .expect("segments inserted");
        drop(store);

        let writer = CheckpointWriter::spawn(db_path.clone(), Arc::new(NullProgressSink));
        let sender = writer.sender();

        sender
            .send(CheckpointCommand::SaveTranslation {
                job_id: job.id.clone(),
                translation: Box::new(test_translation("seg_a", 0, SegmentStatus::Succeeded)),
                provider: "mock".to_string(),
                model: "mock-model".to_string(),
                prompt_version: "v1".to_string(),
            })
            .await
            .expect("send ok");

        sender
            .send(CheckpointCommand::SaveTranslation {
                job_id: job.id.clone(),
                translation: Box::new(test_translation("seg_b", 1, SegmentStatus::NeedsReview)),
                provider: "mock".to_string(),
                model: "mock-model".to_string(),
                prompt_version: "v1".to_string(),
            })
            .await
            .expect("send ok");

        drop(sender);
        writer.shutdown().await.expect("shutdown should succeed");

        let store = JobStore::open(&db_path).expect("re-open ok");
        let summary = store.summary(&job.id).unwrap().expect("summary exists");
        assert_eq!(summary.succeeded, 1, "one succeeded");
        assert_eq!(summary.needs_review, 1, "one needs review");
        assert_eq!(summary.total_segments, 2);

        let _ = fs::remove_file(db_path);
        let _ = fs::remove_file(input_path);
    }

    #[test]
    fn succeeded_translation_persists_warning_finding_round_trip() {
        let db_path = temp_path("warning_round_trip.sqlite");
        let input_path = temp_path("warning_input.epub");
        fs::write(&input_path, b"epub bytes").expect("input fixture writable");

        let store = JobStore::open(&db_path).expect("store open for setup");
        let job = store
            .create_job(CreateJob {
                input: &input_path,
                output: &temp_path("warning_output.epub"),
                source_lang: Some("English"),
                target_lang: "Italian",
                provider: "mock",
                model: "mock-model",
                base_url: None,
                api_key_env: None,
                book_id: None,
                series_id: None,
            })
            .expect("job created");
        store
            .insert_segments(
                &job.id,
                &[test_segment("seg_warning", 0)],
                "v1",
                "mock",
                "mock-model",
                "test_ns",
            )
            .expect("segment inserted");

        let mut translation = test_translation("seg_warning", 0, SegmentStatus::Succeeded);
        translation.error = Some("warning: protected span missing: E=mc^2 [kind=math]".to_string());
        apply(
            &store,
            CheckpointCommand::SaveTranslation {
                job_id: job.id.clone(),
                translation: Box::new(translation),
                provider: "mock".to_string(),
                model: "mock-model".to_string(),
                prompt_version: "v1".to_string(),
            },
        )
        .expect("successful translation with warning should checkpoint");
        drop(store);

        let store = JobStore::open(&db_path).expect("store re-open");
        let summary = store
            .summary(&job.id)
            .expect("summary query")
            .expect("summary exists");
        assert_eq!(summary.succeeded, 1);
        assert_eq!(summary.needs_review, 0);
        assert_eq!(
            store
                .prune_stale_findings(&job.id)
                .expect("pruning stale errors"),
            0,
            "a warning on a succeeded segment is not stale"
        );
        let findings = store
            .segment_qa_findings(&job.id)
            .expect("warning findings load");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].segment_id, "seg_warning");
        assert_eq!(findings[0].kind, "protected_span_missing");
        assert_eq!(findings[0].severity, "warning");
        assert_eq!(
            findings[0].message,
            "protected span missing: E=mc^2 [kind=math]"
        );

        drop(store);
        let _ = fs::remove_file(db_path);
        let _ = fs::remove_file(input_path);
    }

    #[tokio::test]
    async fn checkpoint_send_does_not_emit_segment_finished_before_persistence() {
        let (tx, mut rx) = mpsc::channel::<CheckpointCommand>(CHECKPOINT_QUEUE_CAPACITY);
        let progress = Arc::new(RecordingProgressSink::default());
        let sender = CheckpointSender {
            tx,
            queue_depth: Arc::new(AtomicUsize::new(0)),
            progress: progress.clone(),
        };

        sender
            .send(CheckpointCommand::SaveTranslation {
                job_id: "job".to_string(),
                translation: Box::new(test_translation("seg_a", 0, SegmentStatus::Succeeded)),
                provider: "mock".to_string(),
                model: "mock-model".to_string(),
                prompt_version: "v1".to_string(),
            })
            .await
            .expect("send ok");

        let _queued = rx.try_recv().expect("command should be queued");
        let events = progress.events();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ProgressEvent::CheckpointQueued { .. })),
            "send should still report queued checkpoint work"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, ProgressEvent::SegmentFinished { .. })),
            "SegmentFinished must wait until the writer persists the command"
        );
    }

    #[tokio::test]
    async fn checkpoint_writer_emits_segment_finished_after_persistence() {
        let db_path = temp_path("progress_after_persist.sqlite");
        let input_path = temp_path("input_progress.epub");
        let store = JobStore::open(&db_path).expect("store open for setup");
        fs::write(&input_path, b"epub bytes").expect("input fixture writable");
        let job = store
            .create_job(CreateJob {
                input: &input_path,
                output: &temp_path("output_progress.epub"),
                source_lang: Some("English"),
                target_lang: "Italian",
                provider: "mock",
                model: "mock-model",
                base_url: None,
                api_key_env: None,
                book_id: None,
                series_id: None,
            })
            .expect("job created");
        store
            .insert_segments(
                &job.id,
                &[test_segment("seg_a", 0)],
                "v1",
                "mock",
                "mock-model",
                "test_ns",
            )
            .expect("segments inserted");
        drop(store);

        let progress = Arc::new(RecordingProgressSink::default());
        let writer = CheckpointWriter::spawn(db_path.clone(), progress.clone());
        let sender = writer.sender();
        sender
            .send(CheckpointCommand::SaveTranslation {
                job_id: job.id.clone(),
                translation: Box::new(test_translation("seg_a", 0, SegmentStatus::Succeeded)),
                provider: "mock".to_string(),
                model: "mock-model".to_string(),
                prompt_version: "v1".to_string(),
            })
            .await
            .expect("send ok");

        drop(sender);
        writer.shutdown().await.expect("writer shutdown");

        let store = JobStore::open(&db_path).expect("re-open ok");
        let summary = store.summary(&job.id).unwrap().expect("summary exists");
        assert_eq!(summary.succeeded, 1);
        let events = progress.events();
        let segment_finished_pos = events
            .iter()
            .position(|event| matches!(event, ProgressEvent::SegmentFinished { .. }))
            .expect("segment finished event");
        let checkpoint_flushed_pos = events
            .iter()
            .position(|event| matches!(event, ProgressEvent::CheckpointFlushed { .. }))
            .expect("checkpoint flushed event");
        assert!(
            segment_finished_pos < checkpoint_flushed_pos,
            "SegmentFinished should be emitted by the writer after apply and before flush bookkeeping"
        );

        let _ = fs::remove_file(db_path);
        let _ = fs::remove_file(input_path);
    }

    #[tokio::test]
    async fn checkpoint_writer_surfaces_original_sqlite_error_on_join() {
        let db_path = temp_path("baddb.sqlite");
        fs::write(&db_path, b"not a sqlite file").expect("bad db written");

        let writer = CheckpointWriter::spawn(db_path.clone(), Arc::new(NullProgressSink));
        drop(writer.sender());

        let result = writer.shutdown().await;
        assert!(
            result.is_err(),
            "shutdown must fail when the db cannot be opened"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("checkpoint writer"),
            "error msg ({msg}) should reference checkpoint writer"
        );

        let _ = fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn checkpoint_send_reports_closed_writer() {
        let (tx, rx) = mpsc::channel::<CheckpointCommand>(CHECKPOINT_QUEUE_CAPACITY);
        let sender = CheckpointSender {
            tx,
            queue_depth: Arc::new(AtomicUsize::new(0)),
            progress: Arc::new(NullProgressSink),
        };
        drop(rx);

        let result = sender
            .send(CheckpointCommand::SaveTranslation {
                job_id: "job".to_string(),
                translation: Box::new(test_translation("seg", 0, SegmentStatus::Succeeded)),
                provider: "mock".to_string(),
                model: "mock-model".to_string(),
                prompt_version: "v1".to_string(),
            })
            .await;
        assert!(
            result.is_err(),
            "send must fail when the receiver (writer) has exited"
        );
    }

    #[tokio::test]
    async fn checkpoint_writer_survives_poisoned_command_and_keeps_flushing() {
        let db_path = temp_path("poison.sqlite");
        let input_path = temp_path("input_poison.epub");
        fs::write(&input_path, b"epub bytes").expect("input fixture writable");

        let store = JobStore::open(&db_path).expect("store open for setup");
        let job = store
            .create_job(CreateJob {
                input: &input_path,
                output: &temp_path("output_poison.epub"),
                source_lang: Some("English"),
                target_lang: "Italian",
                provider: "mock",
                model: "mock-model",
                base_url: None,
                api_key_env: None,
                book_id: None,
                series_id: None,
            })
            .expect("job created");
        // Only seg_known exists; the phantom command below violates the
        // translations->segments foreign key.
        store
            .insert_segments(
                &job.id,
                &[test_segment("seg_known", 0)],
                "v1",
                "mock",
                "mock-model",
                "test_ns",
            )
            .expect("segments inserted");
        drop(store);

        let progress = Arc::new(RecordingProgressSink::default());
        let writer = CheckpointWriter::spawn(db_path.clone(), progress.clone());
        let sender = writer.sender();

        sender
            .send(CheckpointCommand::SaveTranslation {
                job_id: job.id.clone(),
                translation: Box::new(test_translation("seg_phantom", 0, SegmentStatus::Succeeded)),
                provider: "mock".to_string(),
                model: "mock-model".to_string(),
                prompt_version: "v1".to_string(),
            })
            .await
            .expect("phantom send ok");
        sender
            .send(CheckpointCommand::SaveTranslation {
                job_id: job.id.clone(),
                translation: Box::new(test_translation("seg_known", 0, SegmentStatus::Succeeded)),
                provider: "mock".to_string(),
                model: "mock-model".to_string(),
                prompt_version: "v1".to_string(),
            })
            .await
            .expect("known send ok");

        drop(sender);
        writer
            .shutdown()
            .await
            .expect("writer must survive a poisoned command");

        let events = progress.events();
        assert!(
            events.iter().any(|event| matches!(
                event,
                ProgressEvent::Error { kind, .. } if kind == "checkpoint_write"
            )),
            "the failed write should be reported as an error event"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                ProgressEvent::Warning { kind, .. } if kind == "checkpoint_dropped_commands"
            )),
            "shutdown should surface how many commands were dropped"
        );

        let store = JobStore::open(&db_path).expect("re-open ok");
        let summary = store.summary(&job.id).unwrap().expect("summary exists");
        assert_eq!(summary.succeeded, 1, "later checkpoints must still persist");
        let _ = fs::remove_file(db_path);
        let _ = fs::remove_file(input_path);
    }

    #[tokio::test]
    async fn checkpoint_channel_applies_backpressure() {
        let db_path = temp_path("backpressure.sqlite");
        let input_path = temp_path("input_bp.epub");
        fs::write(&input_path, b"epub bytes").expect("input writable");

        let store = JobStore::open(&db_path).expect("store open");
        let job = store
            .create_job(CreateJob {
                input: &input_path,
                output: &temp_path("out_bp.epub"),
                source_lang: Some("English"),
                target_lang: "Italian",
                provider: "mock",
                model: "mock-prefix",
                base_url: None,
                api_key_env: None,
                book_id: None,
                series_id: None,
            })
            .expect("job created");
        store
            .insert_segments(
                &job.id,
                &[test_segment("seg_bp", 0)],
                "v1",
                "mock",
                "mock-prefix",
                "test_ns",
            )
            .expect("segments inserted");
        drop(store);

        let writer = CheckpointWriter::spawn(db_path.clone(), Arc::new(NullProgressSink));
        let sender = writer.sender();

        // Send many items faster than the writer can process. Use try_send
        // to detect when the bounded channel fills up.
        let mut filled = false;
        for _ in 0..CHECKPOINT_QUEUE_CAPACITY + 16 {
            match sender.tx.try_send(CheckpointCommand::SaveTranslation {
                job_id: job.id.clone(),
                translation: Box::new(test_translation("seg_bp", 0, SegmentStatus::Succeeded)),
                provider: "mock".to_string(),
                model: "mock-prefix".to_string(),
                prompt_version: "v1".to_string(),
            }) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    filled = true;
                    break;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => break,
            }
        }

        assert!(
            filled,
            "bounded channel with capacity {CHECKPOINT_QUEUE_CAPACITY} should fill up and reject try_send"
        );

        drop(sender);
        writer.shutdown().await.expect("writer shutdown");

        let store = JobStore::open(&db_path).expect("re-open");
        let summary = store.summary(&job.id).unwrap().expect("summary exists");
        assert!(summary.succeeded > 0, "some items should be persisted");

        let _ = fs::remove_file(db_path);
        let _ = fs::remove_file(input_path);
    }

    fn needs_review_fixture(job_id: &str) -> CheckpointCommand {
        let mut translation = test_translation("seg_findings", 0, SegmentStatus::NeedsReview);
        translation.error = Some(
            "error: translation is unchanged from the source-language prose; \
             batch translation block mismatch: missing=[\"b_000026\"], extra=[], duplicate=[]"
                .to_string(),
        );
        CheckpointCommand::SaveTranslation {
            job_id: job_id.to_string(),
            translation: Box::new(translation),
            provider: "mock".to_string(),
            model: "mock-model".to_string(),
            prompt_version: "v1".to_string(),
        }
    }

    #[test]
    fn needs_review_checkpoint_persists_legacy_parsed_findings() {
        let db_path = temp_path("findings_legacy.sqlite");
        let input_path = temp_path("findings_legacy.epub");
        fs::write(&input_path, b"epub bytes").expect("input fixture writable");

        let store = JobStore::open(&db_path).expect("store open for setup");
        let job = store
            .create_job(CreateJob {
                input: &input_path,
                output: &temp_path("findings_legacy_out.epub"),
                source_lang: Some("English"),
                target_lang: "Italian",
                provider: "mock",
                model: "mock-model",
                base_url: None,
                api_key_env: None,
                book_id: None,
                series_id: None,
            })
            .expect("job created");
        store
            .insert_segments(
                &job.id,
                &[test_segment("seg_findings", 0)],
                "v1",
                "mock",
                "mock-model",
                "test_ns",
            )
            .expect("segment inserted");

        apply(&store, needs_review_fixture(&job.id)).expect("needs_review checkpoint applies");

        let findings = store
            .segment_qa_findings(&job.id)
            .expect("findings load back");
        assert_eq!(findings.len(), 2, "legacy error decomposes into two rows");
        let source_copy = findings
            .iter()
            .find(|finding| finding.kind == "source_copy_unchanged")
            .expect("source copy finding");
        // Instance severity, not the legacy "error: " prefix: a source-copy
        // hit is a warning unless the instance overrides it.
        assert_eq!(source_copy.severity, "warning");
        let mismatch = findings
            .iter()
            .find(|finding| finding.kind == "batch_block_mismatch")
            .expect("block mismatch finding");
        assert_eq!(mismatch.severity, "error");

        drop(store);
        let _ = fs::remove_file(db_path);
        let _ = fs::remove_file(input_path);
    }

    #[test]
    fn engine_findings_persist_with_block_id_and_instance_severity() {
        let db_path = temp_path("findings_structured.sqlite");
        let input_path = temp_path("findings_structured.epub");
        fs::write(&input_path, b"epub bytes").expect("input fixture writable");

        let store = JobStore::open(&db_path).expect("store open for setup");
        let job = store
            .create_job(CreateJob {
                input: &input_path,
                output: &temp_path("findings_structured_out.epub"),
                source_lang: Some("English"),
                target_lang: "Italian",
                provider: "mock",
                model: "mock-model",
                base_url: None,
                api_key_env: None,
                book_id: None,
                series_id: None,
            })
            .expect("job created");
        store
            .insert_segments(
                &job.id,
                &[test_segment("seg_findings", 0)],
                "v1",
                "mock",
                "mock-model",
                "test_ns",
            )
            .expect("segment inserted");

        apply(&store, needs_review_fixture(&job.id)).expect("needs_review checkpoint applies");

        // Engine-provided structured findings (the path the checkpoint takes
        // once `SegmentTranslation` carries the engine's `findings`): they
        // replace the legacy-parsed rows and keep block attribution.
        let engine_findings = vec![
            bookforge_core::finding::EngineFinding::new(
                bookforge_core::finding::QaFindingKind::SourceCopyUnchanged,
                "title block copied unchanged",
            )
            .with_block_id("b_000000"),
            bookforge_core::finding::EngineFinding::new(
                bookforge_core::finding::QaFindingKind::BatchBlockMismatch,
                "missing block translations",
            )
            .with_block_id("b_000001"),
        ];
        persist_checkpoint_findings(&store, &job.id, "seg_findings", &engine_findings, None);

        let findings = store
            .segment_qa_findings(&job.id)
            .expect("structured findings load back");
        assert_eq!(findings.len(), 2, "structured rows replace legacy rows");
        let title = findings
            .iter()
            .find(|finding| finding.block_id.as_deref() == Some("b_000000"))
            .expect("block-attributed title finding");
        assert_eq!(title.kind, "source_copy_unchanged");
        assert_eq!(title.severity, "warning", "instance severity is preserved");
        assert_eq!(title.message, "title block copied unchanged");
        let block = findings
            .iter()
            .find(|finding| finding.block_id.as_deref() == Some("b_000001"))
            .expect("block-attributed mismatch finding");
        assert_eq!(block.kind, "batch_block_mismatch");
        assert_eq!(block.severity, "error");

        drop(store);
        let _ = fs::remove_file(db_path);
        let _ = fs::remove_file(input_path);
    }

    #[test]
    fn checkpoint_findings_prefer_engine_findings_over_legacy_parse() {
        let structured = vec![bookforge_core::finding::EngineFinding::new(
            bookforge_core::finding::QaFindingKind::Other,
            "engine reported",
        )];
        assert_eq!(
            findings_for_checkpoint(&structured, Some("legacy error text")),
            structured,
            "non-empty engine findings win unchanged"
        );
        let parsed = findings_for_checkpoint(
            &[],
            Some("error: translation is unchanged from the source-language prose"),
        );
        assert_eq!(parsed.len(), 1);
        assert_eq!(
            parsed[0].kind,
            bookforge_core::finding::QaFindingKind::SourceCopyUnchanged
        );
        assert!(findings_for_checkpoint(&[], None).is_empty());
        assert!(findings_for_checkpoint(&[], Some("   ")).is_empty());
    }
}
