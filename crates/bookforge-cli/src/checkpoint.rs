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

#[allow(dead_code)]
pub enum CheckpointCommand {
    SaveTranslation {
        job_id: String,
        translation: Box<SegmentTranslation>,
        provider: String,
        model: String,
        prompt_version: String,
    },
    MarkFailed {
        job_id: String,
        segment_id: String,
        error: String,
    },
}

impl CheckpointCommand {
    fn segment_id_for_progress(&self) -> Option<String> {
        match self {
            CheckpointCommand::SaveTranslation { translation, .. } => {
                Some(translation.segment_id.0.clone())
            }
            CheckpointCommand::MarkFailed { segment_id, .. } => Some(segment_id.clone()),
        }
    }

    fn segment_finished_event(&self) -> Option<ProgressEvent> {
        let CheckpointCommand::SaveTranslation { translation, .. } = self else {
            return None;
        };
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

            let mut flushed = 0usize;

            while let Some(cmd) = rx.blocking_recv() {
                writer_depth.fetch_sub(1, Ordering::AcqRel);

                let segment_id = cmd.segment_id_for_progress();
                let segment_finished = cmd.segment_finished_event();
                let started = std::time::Instant::now();

                apply(&store, cmd)?;

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

    #[allow(dead_code)]
    pub fn sender_with_progress(&self, progress: Arc<dyn ProgressSink>) -> CheckpointSender {
        CheckpointSender {
            tx: self.tx.clone(),
            queue_depth: self.queue_depth.clone(),
            progress,
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

fn apply(store: &JobStore, cmd: CheckpointCommand) -> Result<()> {
    match cmd {
        CheckpointCommand::SaveTranslation {
            job_id,
            translation,
            provider,
            model,
            prompt_version,
        } => {
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
                }
                SegmentStatus::Failed => {
                    store.mark_segment_failed(
                        &job_id,
                        &translation.segment_id.0,
                        translation.error.as_deref().unwrap_or("translation failed"),
                    )?;
                }
                _ => {}
            }
        }
        CheckpointCommand::MarkFailed {
            job_id,
            segment_id,
            error,
        } => {
            store.mark_segment_failed(&job_id, &segment_id, &error)?;
        }
    }
    Ok(())
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
            .send(CheckpointCommand::MarkFailed {
                job_id: "job".to_string(),
                segment_id: "seg".to_string(),
                error: "some error".to_string(),
            })
            .await;
        assert!(
            result.is_err(),
            "send must fail when the receiver (writer) has exited"
        );
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
}
