//! Checkpoint durability regressions: interrupted partial work stays
//! resumable, every checkpoint commits atomically, and a human correction
//! committed by another process always wins over a late model checkpoint.

use super::*;
use bookforge_core::{
    ir::{BlockId, SectionId},
    segment::{
        Segment, SegmentBlock, SegmentConstraints, SegmentContext, SegmentId, SegmentMetadata,
        SegmentSource, SegmentTextRun,
    },
};
use std::sync::mpsc;

const INTERRUPTED: &str = "translation interrupted before all blocks completed";

fn temp_path(name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "bookforge-checkpoint-test-{}-{}-{}-{name}",
        std::process::id(),
        unix_timestamp_nanos(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

fn segment(id: &str) -> Segment {
    let block_id = BlockId("b_000000".to_string());
    Segment {
        id: SegmentId(id.to_string()),
        section_id: SectionId("sec_000000".to_string()),
        ordinal: 0,
        block_ids: vec![block_id.clone()],
        source: SegmentSource {
            text: "Source text".to_string(),
            blocks: vec![SegmentBlock {
                block_id,
                kind: "paragraph".to_string(),
                text: "Source text".to_string(),
                text_runs: vec![SegmentTextRun {
                    id: "r0".to_string(),
                    text: "Source text".to_string(),
                }],
                protected_spans: Vec::new(),
            }],
            token_estimate: 2,
        },
        context: SegmentContext::default(),
        metadata: SegmentMetadata::default(),
        constraints: SegmentConstraints::default(),
        checksum: "checksum_0".to_string(),
    }
}

fn blocks(text: &str) -> Vec<BlockTranslation> {
    vec![BlockTranslation {
        block_id: BlockId("b_000000".to_string()),
        text: text.to_string(),
    }]
}

/// One job with one segment `seg_a` already checkpointed as `succeeded` with
/// the text "Saved" and 11 input / 7 output tokens.
struct Fixture {
    db_path: PathBuf,
    input_path: PathBuf,
    store: JobStore,
    job_id: String,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let db_path = temp_path(name);
        let input_path = temp_path("input.epub");
        fs::write(&input_path, b"synthetic epub bytes").unwrap();
        let store = JobStore::open(&db_path).unwrap();
        let job = store
            .create_job(CreateJob {
                input: &input_path,
                output: &temp_path("output.epub"),
                source_lang: Some("English"),
                target_lang: "Italian",
                provider: "mock",
                model: "mock-prefix",
                base_url: None,
                api_key_env: None,
                book_id: None,
                series_id: None,
            })
            .unwrap();
        store
            .insert_segments(
                &job.id,
                &[segment("seg_a")],
                "v1",
                "mock",
                "mock-prefix",
                "checkpoint_ns",
            )
            .unwrap();
        Self {
            db_path,
            input_path,
            store,
            job_id: job.id,
        }
    }

    fn with_saved_translation(name: &str) -> Self {
        let fixture = Self::new(name);
        fixture
            .store
            .save_translation(SaveTranslation {
                job_id: &fixture.job_id,
                segment_id: "seg_a",
                translated_text: "Saved",
                blocks: &blocks("Saved"),
                provider: "mock",
                model: "mock-prefix",
                prompt_version: "v1",
                input_tokens: Some(11),
                input_cached_tokens: Some(0),
                output_tokens: Some(7),
                tokens_estimated: false,
            })
            .unwrap();
        fixture
    }

    fn record(&self) -> SegmentRecord {
        self.store.segment_records(&self.job_id).unwrap().remove(0)
    }

    fn job_status(&self) -> String {
        self.store.get_job(&self.job_id).unwrap().unwrap().status
    }

    fn block_text(&self) -> String {
        self.store.load_block_translations(&self.job_id).unwrap()[0]
            .text
            .clone()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.db_path);
        let _ = fs::remove_file(&self.input_path);
    }
}

fn partial_request<'a>(job_id: &'a str, blocks: &'a [BlockTranslation]) -> SaveNeedsReview<'a> {
    SaveNeedsReview {
        job_id,
        segment_id: "seg_a",
        preserved_text: "Partial",
        blocks,
        provider: "mock",
        model: "mock-prefix",
        prompt_version: "v1",
        error: INTERRUPTED,
        input_tokens: Some(10),
        input_cached_tokens: Some(2),
        output_tokens: Some(5),
        tokens_estimated: false,
    }
}

#[derive(Clone, Copy, Debug)]
enum CheckpointKind {
    Successful,
    Cached,
    NeedsReview,
    RetryPending,
    Failed,
}

const ALL_KINDS: [CheckpointKind; 5] = [
    CheckpointKind::Successful,
    CheckpointKind::Cached,
    CheckpointKind::NeedsReview,
    CheckpointKind::RetryPending,
    CheckpointKind::Failed,
];

fn save_checkpoint(store: &JobStore, job_id: &str, kind: CheckpointKind) -> Result<()> {
    let model_blocks = blocks("MODEL OVERWRITE");
    match kind {
        CheckpointKind::Successful => store.save_translation_with_findings(
            SaveTranslation {
                job_id,
                segment_id: "seg_a",
                translated_text: "MODEL OVERWRITE",
                blocks: &model_blocks,
                provider: "mock",
                model: "mock-prefix",
                prompt_version: "v1",
                input_tokens: Some(100),
                input_cached_tokens: Some(20),
                output_tokens: Some(50),
                tokens_estimated: false,
            },
            Some("warning: protected span missing: 2 [kind=number]"),
        ),
        CheckpointKind::Cached => store.save_cached_translation(SaveCachedTranslation {
            job_id,
            segment_id: "seg_a",
            translated_text: "MODEL OVERWRITE",
            blocks: &model_blocks,
            provider: "mock",
            model: "mock-prefix",
            prompt_version: "v1",
        }),
        CheckpointKind::NeedsReview => {
            store.save_needs_review(partial_request(job_id, &model_blocks))
        }
        CheckpointKind::RetryPending => {
            store.save_retry_pending_translation(partial_request(job_id, &model_blocks))
        }
        CheckpointKind::Failed => {
            store.mark_segment_failed(job_id, "seg_a", "provider error: late failure")
        }
    }
}

#[test]
fn interrupted_partial_translation_is_classified_as_an_interruption() {
    let findings = classify_segment_error(INTERRUPTED);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].kind, QaFindingKind::Interrupted);
    assert_eq!(findings[0].severity, QaFindingSeverity::Warning);
}

#[test]
fn retry_pending_checkpoint_keeps_partial_work_resumable() {
    let fixture = Fixture::new("retry_pending.sqlite");
    let partial = blocks("Partial");
    fixture
        .store
        .save_retry_pending_translation(partial_request(&fixture.job_id, &partial))
        .unwrap();

    let record = fixture.record();
    assert_eq!(record.status, "retry_pending");
    assert_eq!(record.attempts, 1);
    assert_eq!(
        (record.input_tokens, record.output_tokens),
        (Some(10), Some(5))
    );
    assert_eq!(fixture.block_text(), "Partial");
    assert_eq!(
        fixture.store.resumable_segment_ids(&fixture.job_id).unwrap(),
        ["seg_a"]
    );
    // An interruption does not claim the job needs review.
    assert_eq!(fixture.job_status(), "running");
    let findings = fixture.store.segment_qa_findings(&fixture.job_id).unwrap();
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].kind, "interrupted");
}

#[test]
fn retry_pending_checkpoint_leaves_dashboard_guidance_for_the_retry() {
    let fixture = Fixture::new("retry_pending_guidance.sqlite");
    fixture.store.mark_job_stopped(&fixture.job_id).unwrap();
    fixture
        .store
        .request_segment_retry(&fixture.job_id, "seg_a", Some("keep the name Ada"))
        .unwrap();
    let partial = blocks("Partial");
    fixture
        .store
        .save_retry_pending_translation(partial_request(&fixture.job_id, &partial))
        .unwrap();
    assert_eq!(
        fixture
            .store
            .load_retry_guidance(&fixture.job_id)
            .unwrap()
            .get("seg_a")
            .map(String::as_str),
        Some("keep the name Ada")
    );

    fixture
        .store
        .save_needs_review(partial_request(&fixture.job_id, &partial))
        .unwrap();
    assert!(
        fixture
            .store
            .load_retry_guidance(&fixture.job_id)
            .unwrap()
            .is_empty(),
        "a completed review attempt consumes the guidance"
    );
}

#[test]
fn incomplete_checkpoints_roll_back_text_blocks_and_status_together() {
    for kind in [CheckpointKind::NeedsReview, CheckpointKind::RetryPending] {
        let fixture = Fixture::with_saved_translation("incomplete_rollback.sqlite");
        fixture
            .store
            .conn
            .borrow()
            .execute_batch(
                "CREATE TRIGGER reject_checkpoint BEFORE UPDATE ON segments
                 BEGIN SELECT RAISE(ABORT, 'checkpoint failure fixture'); END;",
            )
            .unwrap();
        assert!(save_checkpoint(&fixture.store, &fixture.job_id, kind).is_err());

        let saved = fixture
            .store
            .load_terminal_segment_translations(&fixture.job_id)
            .unwrap();
        assert_eq!(saved[0].translated_text, "Saved", "{kind:?}");
        assert_eq!(fixture.block_text(), "Saved", "{kind:?}");
        let record = fixture.record();
        assert_eq!(record.status, "succeeded", "{kind:?}");
        assert_eq!(record.attempts, 1, "{kind:?}");
        assert_eq!(record.input_tokens, Some(11), "{kind:?}");
    }
}

#[test]
fn failed_checkpoint_rolls_back_segment_and_job_together() {
    let fixture = Fixture::new("failed_rollback.sqlite");
    fixture
        .store
        .conn
        .borrow()
        .execute_batch(
            "CREATE TRIGGER reject_job_touch BEFORE UPDATE ON jobs
             BEGIN SELECT RAISE(ABORT, 'job touch failure fixture'); END;",
        )
        .unwrap();

    assert!(
        fixture
            .store
            .mark_segment_failed_if_unfinished(&fixture.job_id, "seg_a", "provider error: boom")
            .is_err()
    );
    let record = fixture.record();
    assert_eq!(
        record.status, "queued",
        "segment state must not commit without the job touch"
    );
    assert_eq!(record.error, None);
    assert!(
        fixture
            .store
            .segment_qa_findings(&fixture.job_id)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn retry_pending_checkpoint_preserves_manual_corrections_and_job_state() {
    let fixture = Fixture::new("retry_pending_freeze.sqlite");
    let partial = blocks("Partial");
    fixture
        .store
        .save_retry_pending_translation(partial_request(&fixture.job_id, &partial))
        .unwrap();
    fixture.store.mark_job_needs_review(&fixture.job_id).unwrap();
    fixture
        .store
        .save_manual_correction(SaveManualCorrection {
            job_id: &fixture.job_id,
            segment_id: "seg_a",
            translated_text: "Human correction",
            blocks: &blocks("Human correction"),
        })
        .unwrap();
    assert_eq!(fixture.job_status(), "succeeded");

    // A late partial checkpoint from a draining worker must not reopen it.
    fixture
        .store
        .save_retry_pending_translation(partial_request(&fixture.job_id, &partial))
        .unwrap();
    fixture
        .store
        .mark_segment_failed(&fixture.job_id, "seg_a", "provider error: late failure")
        .unwrap();
    assert_eq!(fixture.job_status(), "succeeded");
    assert_eq!(fixture.block_text(), "Human correction");
    assert_eq!(fixture.record().status, "succeeded");
    assert!(
        fixture
            .store
            .resumable_segment_ids(&fixture.job_id)
            .unwrap()
            .is_empty()
    );
    assert!(
        fixture
            .store
            .segment_qa_findings(&fixture.job_id)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn findings_writes_skip_a_human_corrected_segment() {
    let fixture = Fixture::with_saved_translation("findings_freeze.sqlite");
    fixture.store.mark_job_stopped(&fixture.job_id).unwrap();
    fixture
        .store
        .save_manual_correction(SaveManualCorrection {
            job_id: &fixture.job_id,
            segment_id: "seg_a",
            translated_text: "Human correction",
            blocks: &blocks("Human correction"),
        })
        .unwrap();

    // The checkpoint writer persists findings after its checkpoint commit, so
    // a correction can land in between. Neither writer may reattach them.
    assert_eq!(
        fixture
            .store
            .record_segment_findings(&fixture.job_id, "seg_a", "provider error: stale")
            .unwrap(),
        0
    );
    assert_eq!(
        fixture
            .store
            .record_segment_engine_findings(
                &fixture.job_id,
                "seg_a",
                &[EngineFinding::new(QaFindingKind::Other, "stale engine finding")],
            )
            .unwrap(),
        0
    );
    assert!(
        fixture
            .store
            .segment_qa_findings(&fixture.job_id)
            .unwrap()
            .is_empty()
    );
}

struct RaceGate {
    waiting: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}

thread_local! {
    static RACE_GATE: RefCell<Option<RaceGate>> = const { RefCell::new(None) };
}

/// Busy handler for the checkpoint connection: on its first busy callback it
/// reports that the checkpoint is blocked on the write lock, then waits until
/// the competing transaction has committed before retrying.
fn race_busy_handler(attempt: i32) -> bool {
    if attempt != 0 {
        return false;
    }
    RACE_GATE.with(|gate| {
        gate.borrow().as_ref().is_some_and(|gate| {
            gate.waiting.send(()).is_ok()
                && gate.release.recv_timeout(Duration::from_secs(10)).is_ok()
        })
    })
}

#[test]
fn checkpoints_preserve_a_correction_committed_while_waiting_for_the_write_lock() {
    for kind in ALL_KINDS {
        let fixture = Fixture::with_saved_translation("checkpoint_race.sqlite");
        fixture.store.mark_job_stopped(&fixture.job_id).unwrap();
        let mut manual = Connection::open(&fixture.db_path).unwrap();
        let tx = manual
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        let (waiting_tx, waiting_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker_db = fixture.db_path.clone();
        let job_id = fixture.job_id.clone();
        let worker = std::thread::spawn(move || {
            RACE_GATE.with(|gate| {
                *gate.borrow_mut() = Some(RaceGate {
                    waiting: waiting_tx,
                    release: release_rx,
                });
            });
            let store = JobStore::open(&worker_db).unwrap();
            store
                .conn
                .borrow()
                .busy_handler(Some(race_busy_handler))
                .unwrap();
            save_checkpoint(&store, &job_id, kind)
        });
        // The callback proves the checkpoint is blocked on this transaction
        // without relying on sleeps. Write the same durable state as
        // `save_manual_correction` inside the held transaction.
        waiting_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        tx.execute(
            "UPDATE translations SET translated_text = 'Human correction',
             provider = 'manual', model = 'manual', origin = 'manual',
             human_corrected = 1, corrected_at = 'manual-time'
             WHERE job_id = ?1 AND segment_id = 'seg_a'",
            params![fixture.job_id],
        )
        .unwrap();
        tx.execute(
            "UPDATE translation_blocks SET translated_text = 'Human correction'
             WHERE job_id = ?1 AND segment_id = 'seg_a'",
            params![fixture.job_id],
        )
        .unwrap();
        tx.commit().unwrap();
        release_tx.send(()).unwrap();
        worker.join().unwrap().unwrap();

        let saved = fixture
            .store
            .load_terminal_segment_translations(&fixture.job_id)
            .unwrap();
        assert_eq!(saved.len(), 1, "{kind:?}");
        assert_eq!(saved[0].translated_text, "Human correction", "{kind:?}");
        assert_eq!(saved[0].blocks[0].text, "Human correction", "{kind:?}");
        assert!(saved[0].human_corrected, "{kind:?}");
        assert_eq!(saved[0].corrected_at.as_deref(), Some("manual-time"));
        let record = fixture.record();
        assert_eq!(record.status, "succeeded", "{kind:?}");
        assert_eq!(record.attempts, 1, "{kind:?}");
        assert_eq!(
            (record.input_tokens, record.output_tokens),
            (Some(11), Some(7)),
            "{kind:?}"
        );
        assert!(
            fixture
                .store
                .segment_qa_findings(&fixture.job_id)
                .unwrap()
                .is_empty(),
            "{kind:?}"
        );
        assert_eq!(fixture.job_status(), "stopped", "{kind:?}");
    }
}
