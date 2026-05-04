use std::path::PathBuf;

use anyhow::Result;
use bookforge_core::segment::SegmentStatus;
use bookforge_llm::SegmentTranslation;
use bookforge_store::{JobStore, SaveNeedsReview, SaveTranslation};
use tokio::{sync::mpsc, task::JoinHandle};

#[allow(dead_code)] // MarkFailed is part of the public API; reserved for direct error paths.
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

pub struct CheckpointWriter {
    tx: mpsc::UnboundedSender<CheckpointCommand>,
    join: JoinHandle<Result<()>>,
}

impl CheckpointWriter {
    /// Spawn a SQLite writer actor on a blocking thread. The actor opens
    /// its own JobStore so the hot async path never blocks on disk I/O.
    pub fn spawn(db_path: PathBuf) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<CheckpointCommand>();
        let join = tokio::task::spawn_blocking(move || -> Result<()> {
            let store = JobStore::open(&db_path)
                .map_err(|err| anyhow::anyhow!("checkpoint writer open failed: {err}"))?;
            while let Some(cmd) = rx.blocking_recv() {
                apply(&store, cmd)?;
            }
            Ok(())
        });
        Self { tx, join }
    }

    pub fn sender(&self) -> mpsc::UnboundedSender<CheckpointCommand> {
        self.tx.clone()
    }

    /// Drop the sender so the actor exits, then await the writer task and
    /// surface any SQLite or join error.
    pub async fn shutdown(self) -> Result<()> {
        let CheckpointWriter { tx, join } = self;
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
                    store.save_translation(SaveTranslation {
                        job_id: &job_id,
                        segment_id: &translation.segment_id.0,
                        translated_text: &joined,
                        blocks: &translation.blocks,
                        provider: &provider,
                        model: &model,
                        prompt_version: &prompt_version,
                        input_tokens: translation.input_tokens,
                        output_tokens: translation.output_tokens,
                    })?;
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
                        output_tokens: translation.output_tokens,
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
    use std::{fs, time::SystemTime};

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
            output_tokens: Some(5),
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
        let nanos = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "bookforge-chk-test-{}-{nanos}-{name}",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn checkpoint_writer_flushes_all_translations_before_shutdown() {
        let db_path = temp_path("flush.sqlite");
        let input_path = temp_path("input.epub");

        // Pre-create a job so FK constraints are satisfied when the writer saves.
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

        let writer = CheckpointWriter::spawn(db_path.clone());
        let tx = writer.sender();

        tx.send(CheckpointCommand::SaveTranslation {
            job_id: job.id.clone(),
            translation: Box::new(test_translation("seg_a", 0, SegmentStatus::Succeeded)),
            provider: "mock".to_string(),
            model: "mock-model".to_string(),
            prompt_version: "v1".to_string(),
        })
        .expect("send ok");

        tx.send(CheckpointCommand::SaveTranslation {
            job_id: job.id.clone(),
            translation: Box::new(test_translation("seg_b", 1, SegmentStatus::NeedsReview)),
            provider: "mock".to_string(),
            model: "mock-model".to_string(),
            prompt_version: "v1".to_string(),
        })
        .expect("send ok");

        drop(tx);
        writer.shutdown().await.expect("shutdown should succeed");

        // Re-open and verify both translations were persisted.
        let store = JobStore::open(&db_path).expect("re-open ok");
        let summary = store.summary(&job.id).unwrap().expect("summary exists");
        assert_eq!(summary.succeeded, 1, "one succeeded");
        assert_eq!(summary.needs_review, 1, "one needs review");
        assert_eq!(summary.total_segments, 2);

        let _ = fs::remove_file(db_path);
        let _ = fs::remove_file(input_path);
    }

    #[tokio::test]
    async fn checkpoint_writer_surfaces_original_sqlite_error_on_join() {
        let db_path = temp_path("baddb.sqlite");
        // Write a plain text file that is not a valid SQLite database.
        fs::write(&db_path, b"not a sqlite file").expect("bad db written");

        let writer = CheckpointWriter::spawn(db_path.clone());
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
    async fn checkpoint_send_fails_when_writer_exits() {
        // Simulate a closed channel (receiver dropped) so sends return Err.
        let (tx, rx) = mpsc::unbounded_channel::<CheckpointCommand>();
        drop(rx);

        let result = tx.send(CheckpointCommand::MarkFailed {
            job_id: "job".to_string(),
            segment_id: "seg".to_string(),
            error: "some error".to_string(),
        });
        assert!(
            result.is_err(),
            "send must fail when the receiver (writer) has exited"
        );
    }
}
