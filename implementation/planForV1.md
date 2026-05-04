# planForV1.md — BookForge v1 Roadmap, Patched for Execution Gotchas

Repository: `JunjoSick/bookforge`  
Purpose: v1-ready roadmap for BookForge with visible progress, reliable persistence, and materially faster translation.

This version patches the roadmap with five execution gotchas that must be handled before Codex starts implementing:

1. Do **not** use async callbacks for checkpoint backpressure. Pass checkpoint senders directly into workers.
2. Batched cache lookup must chunk SQLite `IN (...)` queries.
3. Progress events must be lossy/non-blocking. UI lag must never stall translation.
4. WAL mode creates `-wal`/`-shm` files. Doctor/status should explain and validate them.
5. Repair batches must bypass normal split/retry cascades and drop unresolved items to `NeedsReview`.

This file also incorporates the prior codebase-review findings:

- SQLite WAL/busy-timeout needed for concurrent checkpoint writer + main reader.
- Checkpoint channel is currently unbounded and should become bounded.
- Provider retry policy currently has config fields that are not fully honored by provider backoff.
- N+1 cache lookups should become chunked batched queries.
- QA should run concurrently.
- Prompt parsing and profile/prompt-version identifiers should be centralized and stable.
- Large orchestration branches in `translate.rs` should be extracted to prevent bug divergence.

---

## 0. Non-Negotiable Design Principles

### 0.1 Disk writes are reliable and backpressured

Checkpoint persistence is not optional UI. It must be reliable, bounded, and allowed to slow translation if SQLite falls behind.

Use a bounded `mpsc::channel`, not `unbounded_channel`.

### 0.2 UI events are lossy and non-blocking

Progress is cosmetic/observability, not core correctness. If the terminal renderer or JSON event reporter falls behind, translation must not block.

Use `try_send` for progress events. Drop progress events when the UI channel is full, or aggregate counters in atomics.

### 0.3 Avoid async closure lifetime traps

Do not attempt to retrofit the existing synchronous callback model into generic async closures with HRTB-heavy bounds.

Avoid this:

```rust
F: FnMut(SegmentTranslation) -> Fut,
Fut: Future<Output = Result<(), LlmError>>
```

That pattern is easy to make painful in stable Rust when closures capture references. It commonly fails with “implementation of `FnMut` is not general enough” or future-lifetime errors.

Instead, pass runtime handles directly into schedulers/workers:

```rust
checkpoint_tx: CheckpointSender
progress: Arc<dyn ProgressSink>
```

Workers call:

```rust
checkpoint_tx.send(command).await?;
progress.emit(event); // non-blocking inside implementation
```

This is cleaner, easier to reason about, and naturally uses Tokio backpressure for checkpointing.

### 0.4 Do not let SQLite parameter limits bite cache batching

Batched cache lookup must use chunks. Do not build one enormous `WHERE source_hash IN (?, ?, ... ?)` statement for the whole book.

Use:

```rust
for chunk in segments.chunks(900) {
    // query this chunk
}
```

900 leaves room for other bind parameters on SQLite builds with a 999-variable limit.

### 0.5 Repair is a terminal cleanup pass, not a normal batch

A normal translation batch may split/retry. A repair batch must not enter the same split cascade.

If repair fails or returns invalid JSON:

- mark unresolved items/segments `NeedsReview`;
- emit a repair failure event;
- do not split the repair batch;
- do not recursively repair repair failures.

---

# 1. Current Diagnosis

BookForge has two user-facing problems:

1. No usable progress UI.
2. Translation feels slow, even with non-thinking models.

The deeper engineering causes are:

- provider work is not fully observable;
- retry behavior can amplify latency;
- provider backoff config exists but is not consistently honored;
- checkpointing currently uses an unbounded channel;
- SQLite is used by multiple connections without WAL/busy-timeout setup;
- cache application is N+1;
- QA is sequential;
- batch sizing is static;
- progress is absent;
- duplicated orchestration branches make fixes drift.

V1 must fix the reliability and backpressure layer first, then add observability, then tune throughput.

---

# 2. Phase 0 — Storage, Backpressure, Retry, and Batch Safety

This phase comes before UI. A progress bar over broken persistence is not a product.

---

## 2.1 Enable SQLite WAL, busy timeout, and foreign keys on every connection

### Problem

The checkpoint writer opens a separate SQLite connection while the main CLI flow reads summaries, pending segments, cache state, and reports. Without WAL and a busy timeout, SQLite’s default rollback journal can cause `SQLITE_BUSY` errors under load.

### Files

```text
crates/bookforge-store/src/db.rs
crates/bookforge-cli/src/checkpoint.rs
```

### Implementation

In `JobStore::open`, change from:

```rust
let store = Self {
    conn: RefCell::new(Connection::open(&path)?),
    path,
};
store.migrate()?;
```

to:

```rust
let conn = Connection::open(&path)?;

conn.busy_timeout(std::time::Duration::from_secs(5))?;

conn.pragma_update(None, "journal_mode", "WAL")?;
conn.pragma_update(None, "synchronous", "NORMAL")?;
conn.pragma_update(None, "foreign_keys", "ON")?;

let store = Self {
    conn: RefCell::new(conn),
    path,
};

store.migrate()?;
```

Keep `PRAGMA foreign_keys = ON` in `migrate()` if desired, but do not rely on migration only. Foreign keys are per connection.

### WAL file expectations

WAL mode creates:

```text
jobs.sqlite-wal
jobs.sqlite-shm
```

These are not corruption. They are normal SQLite sidecars. If BookForge is force-killed, they can remain on disk until the next clean checkpoint.

Add this to `doctor` and/or `status`:

```text
SQLite:
  database: .bookforge/jobs.sqlite
  WAL mode: enabled
  sidecar files: jobs.sqlite-wal, jobs.sqlite-shm present
  integrity_check: ok
  note: WAL sidecar files are normal and will be recovered automatically.
```

### Doctor behavior

`bookforge doctor --storage` or general `bookforge doctor` should:

1. check whether `.bookforge/jobs.sqlite` exists;
2. check for `-wal` and `-shm`;
3. open the DB;
4. run:

```sql
PRAGMA integrity_check;
PRAGMA journal_mode;
PRAGMA wal_checkpoint(PASSIVE);
```

5. report whether sidecar files are normal or suspicious.

Do not delete WAL files blindly.

### Tests

```rust
#[test]
fn job_store_enables_wal_and_busy_timeout()

#[test]
fn job_store_enables_foreign_keys_on_every_connection()

#[tokio::test]
async fn checkpoint_writer_and_reader_do_not_immediately_busy_fail()

#[test]
fn doctor_reports_wal_sidecars_as_normal_when_integrity_check_passes()
```

---

## 2.2 Replace unbounded checkpoint channel with bounded backpressure

### Problem

Current `CheckpointWriter` uses `mpsc::unbounded_channel`. If provider results arrive faster than SQLite can commit, RAM grows without bound.

### Critical gotcha

Do **not** solve this with generic async callbacks. Async closure bounds will be brittle.

Instead, abandon checkpoint callback pattern and pass a concrete `CheckpointSender` directly into schedulers/workers.

### Files

```text
crates/bookforge-cli/src/checkpoint.rs
crates/bookforge-cli/src/commands/translate.rs
crates/bookforge-llm/src/batch.rs
crates/bookforge-llm/src/scheduler.rs
```

### New types

```rust
pub const CHECKPOINT_QUEUE_CAPACITY: usize = 64;

pub struct CheckpointWriter {
    tx: mpsc::Sender<CheckpointCommand>,
    join: JoinHandle<anyhow::Result<()>>,
    queue_depth: Arc<AtomicUsize>,
}

#[derive(Clone)]
pub struct CheckpointSender {
    tx: mpsc::Sender<CheckpointCommand>,
    queue_depth: Arc<AtomicUsize>,
    progress: Arc<dyn ProgressSink>,
}
```

Spawn:

```rust
pub fn spawn(db_path: PathBuf, progress: Arc<dyn ProgressSink>) -> Self {
    let (tx, mut rx) = mpsc::channel::<CheckpointCommand>(CHECKPOINT_QUEUE_CAPACITY);
    let queue_depth = Arc::new(AtomicUsize::new(0));

    let writer_depth = queue_depth.clone();
    let writer_progress = progress.clone();

    let join = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let store = JobStore::open(&db_path)
            .map_err(|err| anyhow::anyhow!("checkpoint writer open failed: {err}"))?;

        let mut flushed = 0usize;

        while let Some(cmd) = rx.blocking_recv() {
            writer_depth.fetch_sub(1, Ordering::AcqRel);

            let segment_id = cmd.segment_id_for_progress();
            let started = std::time::Instant::now();

            apply(&store, cmd)?;

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

    Self { tx, join, queue_depth }
}
```

Sender:

```rust
impl CheckpointSender {
    pub async fn send(&self, cmd: CheckpointCommand) -> Result<(), LlmError> {
        self.tx
            .send(cmd)
            .await
            .map_err(|_| LlmError::Provider(
                "checkpoint queue closed; checkpoint writer may have failed".to_string()
            ))?;

        let queued = self.queue_depth.fetch_add(1, Ordering::AcqRel) + 1;
        self.progress.emit(ProgressEvent::CheckpointQueued {
            queued,
            timestamp_ms: now_ms(),
        });

        Ok(())
    }
}
```

Prefer incrementing queue depth before send if you want exact ordering, but be careful to decrement on send failure. Simpler acceptable path:

```rust
let queued = self.queue_depth.fetch_add(1, Ordering::AcqRel) + 1;
match self.tx.send(cmd).await {
    Ok(()) => emit queued,
    Err(err) => {
        self.queue_depth.fetch_sub(1, Ordering::AcqRel);
        return Err(...);
    }
}
```

### Scheduler integration

Replace:

```rust
translate_batches_with_callback(..., |translation| send_checkpoint(...))
```

with:

```rust
translate_batches(
    provider,
    batches,
    segments,
    config,
    telemetry,
    limiter,
    checkpoint_sender.clone(),
    progress.clone(),
).await
```

Inside worker/coordinator:

```rust
checkpoint_sender.send(CheckpointCommand::SaveTranslation {
    job_id: job_id.clone(),
    translation: Box::new(translation),
    provider: provider.clone(),
    model: model.clone(),
    prompt_version: prompt_version.clone(),
}).await?;
```

The checkpoint send is now part of worker/coordinator flow and naturally backpressures if SQLite falls behind.

### Tests

```rust
#[tokio::test]
async fn checkpoint_channel_applies_backpressure()

#[tokio::test]
async fn checkpoint_writer_flushes_bounded_queue_before_shutdown()

#[tokio::test]
async fn checkpoint_send_reports_closed_writer()

#[tokio::test]
async fn scheduler_sends_checkpoints_without_async_closure_lifetime_bounds()
```

---

## 2.3 Make progress events non-blocking and lossy

### Problem

If progress events use a bounded channel with `.send().await`, a slow terminal or SSH connection can block translation workers. UI must never throttle LLM calls or SQLite writes.

### Files

```text
crates/bookforge-core/src/progress.rs
crates/bookforge-cli/src/progress.rs
```

### Design

Use a non-blocking progress sink:

```rust
pub trait ProgressSink: Send + Sync + 'static {
    fn emit(&self, event: ProgressEvent);
}
```

CLI channel sink:

```rust
pub struct ChannelProgressSink {
    tx: tokio::sync::mpsc::Sender<ProgressEvent>,
    dropped: Arc<AtomicUsize>,
}

impl ProgressSink for ChannelProgressSink {
    fn emit(&self, event: ProgressEvent) {
        match self.tx.try_send(event) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}
```

Progress channel capacity:

```rust
const PROGRESS_EVENT_QUEUE_CAPACITY: usize = 2048;
```

This is large enough for bursts but finite.

### Event coalescing

For high-frequency events, the UI reporter can coalesce:

- only render every 250ms;
- aggregate counts in `ProgressState`;
- drop old visual-only events if needed.

Disk checkpoint events and provider metrics should be persisted through telemetry/DB if they are critical. The UI event stream is best-effort observability.

### JSONL note

If `--ui json` is selected, stdout can be slow. Still do not block workers. Events flow through the same lossy queue. If the user needs authoritative metrics, use telemetry/report data, not JSON event completeness.

### Tests

```rust
#[tokio::test]
async fn progress_sink_drops_events_when_channel_full_instead_of_blocking()

#[tokio::test]
async fn slow_progress_reporter_does_not_block_translation_worker()
```

---

## 2.4 Make provider retry policy honor runtime config

### Problem

`ProviderRuntimeConfig` contains `retry_after_policy` and `max_backoff_seconds`, but provider code currently relies on hardcoded backoff behavior.

### Files

```text
crates/bookforge-core/src/config.rs
crates/bookforge-llm/src/provider.rs
crates/bookforge-cli/src/commands/translate.rs
```

### Add fields to `OpenAiCompatibleConfig`

```rust
pub struct OpenAiCompatibleConfig {
    pub base_url: String,
    pub api_key_env: String,
    pub model: String,
    pub timeout_seconds: u64,
    pub provider_max_attempts: usize,
    pub thinking_disabled: bool,
    pub retry_after_policy: RetryAfterPolicy,
    pub max_backoff_seconds: u64,
    pub max_idle_per_host: usize,
}
```

Thread values from `settings.provider`.

### Retry delay function

```rust
fn retry_delay(
    policy: RetryAfterPolicy,
    attempt: usize,
    retry_after: Option<Duration>,
    max_backoff: Duration,
) -> Option<Duration> {
    match policy {
        RetryAfterPolicy::None => None,

        RetryAfterPolicy::RespectHeader => {
            retry_after.or_else(|| Some(exponential_delay(attempt).min(max_backoff)))
        }

        RetryAfterPolicy::Fixed => {
            Some(Duration::from_millis(750).min(max_backoff))
        }

        RetryAfterPolicy::JitteredExponential => {
            let base = exponential_delay(attempt).min(max_backoff);
            Some(apply_jitter(base, attempt))
        }
    }
}
```

No retry if `None`.

For deterministic jitter without `rand`:

```rust
fn apply_jitter(base: Duration, attempt: usize) -> Duration {
    let millis = base.as_millis() as u64;
    let spread = millis / 5; // 20%
    let offset = ((attempt as u64 * 1103515245 + 12345) % (spread.max(1))) as u64;
    Duration::from_millis(millis.saturating_sub(spread / 2).saturating_add(offset))
}
```

### Tests

```rust
#[test]
fn retry_policy_none_disables_delay()

#[test]
fn retry_policy_respect_header_uses_retry_after()

#[test]
fn retry_policy_caps_to_max_backoff()

#[test]
fn retry_policy_jittered_exponential_is_bounded()

#[tokio::test]
async fn provider_uses_configured_retry_policy()
```

---

## 2.5 Add single-item invalid-response circuit breaker

### Problem

When a batch fails with invalid JSON, multi-item batches should split. But a single-item batch cannot split further. It must not loop or retry through split logic indefinitely.

### Files

```text
crates/bookforge-llm/src/batch.rs
```

### Explicit branch

```rust
Ok((batch, Err(LlmError::InvalidResponse(error)))) if batch.items.len() == 1 => {
    progress.emit(ProgressEvent::Warning {
        kind: "single_item_batch_invalid_response".to_string(),
        message: format!(
            "single-item batch {} failed with invalid response; not splitting further",
            batch.id
        ),
        timestamp_ms: now_ms(),
    });

    all_results.push(BatchTranslationResult {
        batch_id: batch.id.clone(),
        translations: Vec::new(),
        failures: batch.items.iter().map(|item| BatchItemFailure {
            item_id: item.item_id.clone(),
            segment_id: item.segment_id.clone(),
            error: format!("single-item batch invalid response: {error}"),
        }).collect(),
        input_tokens: None,
        output_tokens: None,
    });
}
```

Keep split branch only for:

```rust
batch.items.len() > 1
```

### Tests

```rust
#[tokio::test]
async fn single_item_invalid_batch_does_not_split_forever()

#[tokio::test]
async fn multi_item_invalid_batch_splits_until_singletons()
```

---

## 2.6 Tag repair batches and bypass split/retry cascades

### Problem

The repair pass groups failed items into one repair batch. If repair returns invalid JSON, it must not enter the normal split/retry path. Repair is terminal cleanup.

### Files

```text
crates/bookforge-llm/src/batch.rs
```

### Add batch kind

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BatchKind {
    Translation,
    Repair,
}
```

Add to `TranslationBatch`:

```rust
pub kind: BatchKind,
```

Default translation batches:

```rust
kind: BatchKind::Translation
```

Repair batch:

```rust
kind: BatchKind::Repair
```

### Rule

In any split/retry handling:

```rust
if batch.kind == BatchKind::Repair {
    // do not split
    // do not retry through normal batch cascade
    // unresolved items become NeedsReview
}
```

Repair failure handling:

```rust
match repair_response {
    Ok(repaired) => apply repaired items,
    Err(error) => {
        progress.emit(ProgressEvent::BatchRepairFinished {
            repaired_items: 0,
            still_failed_items: repair_items.len(),
            latency_ms,
            timestamp_ms: now_ms(),
        });

        for (_, item) in repair_items {
            mark item/segment NeedsReview with:
            "batch repair failed: {error}"
        }
    }
}
```

### Tests

```rust
#[tokio::test]
async fn repair_batch_invalid_json_does_not_split()

#[tokio::test]
async fn repair_batch_failure_marks_items_needs_review()

#[tokio::test]
async fn repair_batch_is_not_repaired_recursively()
```

---

## 2.7 Cap max output tokens against context/user limits

### Problem

`batch_max_output_tokens` and non-batch `max_output_tokens` may request too much output. If prompt + max_tokens exceeds the model context window, providers can return 400s or degrade routing.

### Files

```text
crates/bookforge-core/src/config.rs
crates/bookforge-llm/src/batch.rs
crates/bookforge-llm/src/scheduler.rs
crates/bookforge-cli/src/commands/translate.rs
```

### Config

Add:

```rust
pub struct ProviderRuntimeConfig {
    ...
    pub model_context_tokens: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub batch_max_output_tokens: Option<u32>,
}
```

CLI:

```rust
#[arg(long)]
pub model_context_tokens: Option<u32>,

#[arg(long)]
pub max_output_tokens: Option<u32>,

#[arg(long)]
pub batch_max_output_tokens: Option<u32>,
```

### Budget function

```rust
pub fn cap_output_tokens(
    computed: u32,
    estimated_prompt_tokens: usize,
    model_context_tokens: Option<u32>,
    user_cap: Option<u32>,
) -> u32 {
    let mut out = computed;

    if let Some(context) = model_context_tokens {
        let prompt = estimated_prompt_tokens as u32;
        let remaining = context.saturating_sub(prompt);
        let safe_remaining = remaining.saturating_sub(256);
        out = out.min(safe_remaining.max(512));
    }

    if let Some(cap) = user_cap {
        out = out.min(cap);
    }

    out.max(256)
}
```

Emit warning if cap reduces by a lot.

### Tests

```rust
#[test]
fn output_token_budget_respects_model_context_window()

#[test]
fn output_token_budget_respects_user_cap()

#[test]
fn output_token_budget_never_underflows()
```

---

# 3. Phase 1 — Local High-ROI Refactors

These reduce runtime cost and bug drift before larger features.

## 3.1 Centralize marker parsing

Move duplicate `extract_marker_id`, `is_marker_token`, and marker scanning into:

```text
crates/bookforge-core/src/marker.rs
```

Export from core.

Use in:

```text
crates/bookforge-epub/src/writer.rs
crates/bookforge-llm/src/scheduler.rs
```

Tests:

```rust
extracts_marker_id_from_m_tag
extracts_marker_id_from_keep_tag
extracts_marker_id_from_ref_tag
rejects_missing_or_unquoted_id
```

---

## 3.2 Typed prompt versions and stable profile namespace strings

Add:

```rust
pub enum PromptVersion {
    V1,
    BatchV1,
}
```

with:

```rust
as_str()
```

Add stable profile namespace:

```rust
impl TranslationProfile {
    pub fn namespace_str(self) -> &'static str { ... }
}
```

Remove all `format!("{:?}", settings.profile)` uses from cache namespace construction.

Tests:

```rust
profile_namespace_is_stable_lowercase
prompt_version_as_str_is_stable
cache_namespace_does_not_depend_on_debug_format
```

---

## 3.3 Chunked batched cache lookup

### Problem

N+1 cache lookup is slow, but one giant `IN` query can exceed SQLite parameter limits.

### Files

```text
crates/bookforge-store/src/db.rs
crates/bookforge-cli/src/commands/translate.rs
```

### Request type

```rust
pub struct CacheLookupRequest<'a> {
    pub prompt_version: &'a str,
    pub provider: &'a str,
    pub model: &'a str,
    pub source_lang: Option<&'a str>,
    pub target_lang: &'a str,
    pub cache_namespace: &'a str,
}
```

### API

```rust
pub fn find_cached_translations_batch(
    &self,
    segments: &[Segment],
    request: CacheLookupRequest<'_>,
) -> Result<HashMap<String, CachedTranslation>>
```

### Chunking

Use:

```rust
const SQLITE_IN_CHUNK_SIZE: usize = 900;

for chunk in segments.chunks(SQLITE_IN_CHUNK_SIZE) {
    // build IN (?, ?, ...)
}
```

Do not exceed 900 segment hashes per statement because other bind parameters are also needed.

### Preserve compatibility checks

For every hit:

- namespace must match;
- prompt/provider/model/language must match;
- exact block-ID set must match;
- returned block translations must be ordered by current `segment.block_ids`.

Tests:

```rust
batched_cache_lookup_returns_same_results_as_per_segment_lookup
batched_cache_lookup_chunks_over_sqlite_parameter_limit
batched_cache_lookup_rejects_block_layout_mismatch
batched_cache_lookup_orders_blocks_by_current_segment_order
```

---

## 3.4 PromptLibrary global cache

Add:

```rust
impl PromptLibrary {
    pub fn global() -> &'static PromptLibrary {
        static LIBRARY: OnceLock<PromptLibrary> = OnceLock::new();
        LIBRARY.get_or_init(PromptLibrary::embedded)
    }
}
```

Use in batch/scheduler.

---

## 3.5 Reduce segment/config clones

Use `Arc<Segment>` or indices instead of deep-cloning segments into every task. Wrap `TranslationRunConfig` in `Arc`.

---

## 3.6 Parallel QA

Current QA should run concurrently with a semaphore/worker pool.

Add:

```rust
pub async fn qa_segments_with_concurrency<P>(
    provider: P,
    segments: &[Segment],
    translations: &[SegmentTranslation],
    config: &TranslationRunConfig,
    concurrency: usize,
) -> Vec<QaSegmentReview>
```

Tests:

```rust
qa_segments_runs_with_configured_concurrency
qa_segments_preserves_all_reviews
```

---

## 3.7 Extract common translation pipeline

Batch/non-batch branches in `translate.rs` should share:

- cache application;
- pending segment lookup;
- writer setup;
- QA;
- fallback;
- double-check;
- finish marking;
- rebuild/report.

Create a common pipeline function with an `ExecutionMode` enum.

---

# 4. Phase 2 — Progress UI and Event Log

## 4.1 Dependencies

Workspace:

```toml
indicatif = "0.18"
console = "0.16"
humantime = "2.1"
is-terminal = "0.4"
```

CLI:

```toml
indicatif.workspace = true
console.workspace = true
humantime.workspace = true
is-terminal.workspace = true
```

---

## 4.2 Progress event model

Create:

```text
crates/bookforge-core/src/progress.rs
```

Add events:

- `JobCreated`
- `StageStarted`
- `StageFinished`
- `RuntimeConfigResolved`
- `SegmentationFinished`
- `CacheScanFinished`
- `CacheMissSummary`
- `BatchQueued`
- `BatchSplit`
- `BatchRepairStarted`
- `BatchRepairFinished`
- `RequestStarted`
- `RequestFinished`
- `SegmentFinished`
- `CheckpointQueued`
- `CheckpointFlushed`
- `ConcurrencyChanged`
- `BatchSizingChanged`
- `ArtifactWritten`
- `Warning`
- `Error`

The `ProgressSink::emit` method must be synchronous and non-blocking.

---

## 4.3 CLI ProgressReporter

Create:

```text
crates/bookforge-cli/src/progress.rs
```

CLI modes:

```rust
pub enum UiMode {
    Auto,
    Progress,
    Json,
    Quiet,
}
```

Add flags:

```rust
#[arg(long, value_enum, default_value_t = UiMode::Auto)]
pub ui: UiMode,

#[arg(long)]
pub progress_jsonl: Option<PathBuf>,

#[arg(long, default_value_t = false)]
pub no_eta: bool,
```

Reporter:

```rust
pub struct ProgressReporter {
    tx: mpsc::Sender<ProgressEvent>,
    join: JoinHandle<anyhow::Result<()>>,
    dropped_events: Arc<AtomicUsize>,
}
```

`ProgressReporter::sink()` returns a `ChannelProgressSink` that uses `try_send`.

Render with `indicatif::MultiProgress`, refreshing at most every 250ms.

---

## 4.4 Dashboard content

Show:

- current stage;
- segments done/cached/pending;
- batch progress;
- active provider requests;
- target concurrency;
- p50/p95 latency;
- rate limits;
- timeouts;
- invalid JSON;
- truncations;
- retries;
- checkpoint queue depth/flushed count;
- token throughput;
- ETA;
- dropped progress event count if nonzero.

ETA:

```text
done = cached_segments + completed_segments
rate = done / elapsed_seconds
eta = (total_segments - done) / rate
```

---

## 4.5 JSONL events

Default:

```text
.bookforge/runs/<job_id>/events.jsonl
```

If user passes `--progress-jsonl`, use that.

Because the job ID is only known after job creation, either:

- open JSONL after `JobCreated`; or
- buffer early events in reporter until job ID arrives.

Simplest: start reporter immediately, but open default JSONL when `JobCreated` arrives.

---

# 5. Phase 3 — Wire Events Through the Pipeline

## 5.1 Translate stage events

Emit around:

- read EPUB;
- segmentation;
- job creation;
- cache scan;
- translation;
- fallback;
- QA;
- double-check;
- rebuild;
- report;
- done.

Emit runtime config and retry amplification warnings.

---

## 5.2 Batch events

Batch scheduler should emit:

- `BatchQueued`;
- `RequestStarted`;
- `RequestFinished`;
- `BatchSplit`;
- `BatchRepairStarted`;
- `BatchRepairFinished`;
- `SegmentFinished`;
- warnings for single-item invalid batches;
- warnings/errors for repair failure.

Since checkpoint sender is passed directly, no callback is needed.

---

## 5.3 Non-batch events

Non-batch scheduler should emit:

- request started/finished per attempt;
- segment finished;
- retry attempts;
- truncations.

---

## 5.4 Checkpoint events

Checkpoint sender/writer should emit:

- queued;
- flushed;
- writer error.

Use progress `try_send`, not blocking send.

---

# 6. Phase 4 — v1 Speed Features

## 6.1 Add `v1-fast` profile

Add:

```rust
TranslationProfile::V1Fast
```

Defaults:

```text
batch: enabled
batch target: 16k
max items: 128
scheduler concurrency: 32
scheduler attempts: 1
provider attempts: 1
validation attempts: 1
compact prompts: true
adaptive concurrency: true
adaptive batch sizing: true
thinking disabled: true
QA: off by default
double-check: off by default
```

---

## 6.2 Provider presets

Add:

```rust
ProviderPreset {
    Auto,
    OpenRouterFree,
    OpenRouterPaidFast,
    DeepSeekFree,
    DeepSeekPaid,
    GeminiFlashLite,
    Custom,
}
```

Apply before explicit CLI overrides.

Do not infer free vs paid tier.

---

## 6.3 Provider doctor

Add:

```bash
bookforge doctor --provider openrouter --model google/gemini-2.5-flash-lite
```

Checks:

- API key env var;
- base URL;
- tiny completion latency;
- JSON response_format support;
- usage token support;
- reasoning detection;
- DB/WAL health;
- recommended preset.

Also add:

```bash
bookforge doctor --storage
```

Storage doctor should explain WAL sidecars and run integrity checks.

---

## 6.4 Windowed adaptive concurrency

Replace per-success growth with windowed controller:

```text
429 -> halve immediately
timeout -> reduce to 75%
p95 too high -> reduce 10-20%
stable success window -> +1 every 2-5s
```

Emit `ConcurrencyChanged`.

---

## 6.5 Adaptive batch sizing

Add `BatchSizer`.

Rules:

```text
truncation -> target *= 0.65, max_items *= 0.75
invalid JSON -> target *= 0.75, max_items *= 0.85
p95 too high -> target *= 0.85
stable success -> target *= 1.10
```

Clamp:

```text
Plain/Turbo: 4k..32k
MarkerSafe: 2k..16k
RunPreserving: 1k..8k
```

Repair batches are excluded from adaptive split/retry logic.

---

## 6.6 Compact prompts

Add compact prompt variants:

- `batch_plain_compact_v1`
- `batch_marker_safe_compact_v1`
- `batch_run_preserving_compact_v1`
- `batch_repair_compact_v1`

Use when `settings.compact_prompts`.

---

## 6.7 JSON mode control

Add:

```rust
JsonMode {
    Auto,
    ResponseFormat,
    PromptOnly,
}
```

In `Auto`, if provider rejects `response_format` with unsupported 400, retry once prompt-only and remember for the run.

---

## 6.8 HTTP client pooling

Add to config:

```rust
max_idle_per_host: usize
```

Use:

```rust
reqwest::Client::builder()
    .connect_timeout(Duration::from_secs(30))
    .timeout(Duration::from_secs(effective_timeout))
    .pool_idle_timeout(Duration::from_secs(90))
    .pool_max_idle_per_host(config.max_idle_per_host)
    .tcp_keepalive(Duration::from_secs(60))
    .build()?;
```

---

# 7. Phase 5 — Worker Queues and Cancellation

## 7.1 Worker queue scheduler

After direct checkpoint sender integration, use bounded work/result queues.

Batch:

```rust
let (work_tx, work_rx) = mpsc::channel::<TranslationBatch>(queue_size);
let (result_tx, result_rx) = mpsc::channel::<BatchWorkerResult>(queue_size);
```

Workers process batches, send checkpoints, and emit events.

Coordinator handles:

- split;
- terminal singleton failure;
- transient retry;
- repair;
- aggregation.

Non-batch analogous with segment work items.

---

## 7.2 Graceful Ctrl-C

Add:

```toml
tokio-util = "0.7"
```

Use `CancellationToken`.

On Ctrl-C:

1. stop queueing new work;
2. wait for active requests up to grace period;
3. flush checkpoint writer;
4. mark job `interrupted`;
5. print resume instructions.

CLI:

```rust
#[arg(long, default_value_t = 20)]
pub shutdown_grace_seconds: u64
```

---

# 8. Phase 6 — Extra Commands

## 8.1 `bookforge status <job_id>`

Shows:

- status;
- input/output;
- segment counts;
- token counts;
- last update;
- cache namespace;
- event log path if available.

## 8.2 `bookforge tail <job_id>`

Reads:

```text
.bookforge/runs/<job_id>/events.jsonl
```

and reconstructs dashboard.

---

# 9. Phase 7 — Reports and Cost

## 9.1 Performance report

Add:

```rust
pub struct PerformanceReport {
    pub elapsed_ms: u64,
    pub request_count: usize,
    pub p50_latency_ms: u64,
    pub p95_latency_ms: u64,
    pub rate_limits: usize,
    pub timeouts: usize,
    pub invalid_responses: usize,
    pub truncations: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub blocks_per_minute: f64,
    pub output_tokens_per_minute: f64,
}
```

Add markdown section to reports.

## 9.2 Cost

Keep f64 cost estimate for v1 display only. Label as estimate. If strict billing is ever needed, switch to `rust_decimal`.

---

# 10. Tests

## 10.1 Storage and checkpoint tests

```rust
job_store_enables_wal_and_busy_timeout
job_store_enables_foreign_keys_on_every_connection
doctor_reports_wal_sidecars_as_normal
checkpoint_channel_applies_backpressure
checkpoint_writer_flushes_bounded_queue_before_shutdown
checkpoint_send_reports_closed_writer
scheduler_sends_checkpoints_without_async_closure_lifetime_bounds
```

## 10.2 Progress tests

```rust
progress_sink_drops_events_when_channel_full_instead_of_blocking
slow_progress_reporter_does_not_block_translation_worker
progress_reporter_writes_jsonl_events
progress_reporter_handles_quiet_mode
progress_reporter_json_mode_outputs_valid_json_lines
```

## 10.3 Retry tests

```rust
retry_policy_none_disables_delay
retry_policy_respect_header_uses_retry_after
retry_policy_caps_to_max_backoff
provider_uses_configured_retry_policy
```

## 10.4 Batch tests

```rust
single_item_invalid_batch_does_not_split_forever
multi_item_invalid_batch_splits_until_singletons
repair_batch_invalid_json_does_not_split
repair_batch_failure_marks_items_needs_review
repair_batch_is_not_repaired_recursively
partial_batch_failure_without_successful_repair_marks_segment_needs_review
```

## 10.5 Cache tests

```rust
batched_cache_lookup_returns_same_results_as_per_segment_lookup
batched_cache_lookup_chunks_over_sqlite_parameter_limit
batched_cache_lookup_rejects_block_layout_mismatch
batched_cache_lookup_orders_blocks_by_current_segment_order
```

## 10.6 Performance controller tests

```rust
rate_controller_halves_on_429
rate_controller_reduces_on_timeout
rate_controller_grows_slowly_on_success_window
rate_controller_does_not_grow_on_every_success
batch_sizer_reduces_after_truncation
batch_sizer_reduces_after_invalid_json
batch_sizer_increases_after_stable_success
```

---

# 11. Implementation Order

Use this exact order:

```text
0.1 SQLite WAL + busy_timeout + foreign keys
0.2 bounded checkpoint channel with direct CheckpointSender plumbing
0.3 non-blocking lossy progress sink
0.4 provider retry policy honoring RetryAfterPolicy/max_backoff_seconds
0.5 single-item invalid batch circuit breaker
0.6 repair batches tagged terminal/no split
0.7 max_output_tokens context/user caps

1.1 central marker parsing
1.2 PromptVersion enum + stable profile namespace strings
1.3 chunked batched cache lookup
1.4 PromptLibrary OnceLock
1.5 reduce segment/config clones
1.6 parallel QA
1.7 extract common translation pipeline

2.1 progress dependencies
2.2 ProgressEvent model
2.3 ProgressReporter with --ui auto/progress/json/quiet
2.4 JSONL event logs and dashboard

3.1 stage/cache/runtime events
3.2 batch events
3.3 non-batch events
3.4 checkpoint events

4.1 v1-fast profile
4.2 provider presets
4.3 provider doctor + storage doctor
4.4 windowed adaptive concurrency
4.5 adaptive batch sizing
4.6 compact prompt variants
4.7 JSON mode control
4.8 HTTP client pooling

5.1 worker queue scheduler
5.2 graceful Ctrl-C cancellation

6.1 status command
6.2 tail command

7.1 performance report
7.2 cost estimate labeling
```

---

# 12. V1 Acceptance Checklist

## Persistence

- [ ] WAL enabled.
- [ ] Busy timeout set.
- [ ] Foreign keys enabled per connection.
- [ ] Bounded checkpoint channel.
- [ ] Direct `CheckpointSender` passed into schedulers/workers.
- [ ] No async callback HRTB/lifetime trap.
- [ ] Checkpoint writer errors surfaced.
- [ ] WAL sidecars explained by doctor/status.

## Progress

- [ ] Progress sink uses `try_send`, never blocking workers.
- [ ] Dropped progress events counted.
- [ ] `--ui progress` works.
- [ ] `--ui json` works.
- [ ] `--ui quiet` works.
- [ ] JSONL log exists.
- [ ] ETA visible.
- [ ] Throughput visible.
- [ ] Provider errors visible.
- [ ] Checkpoint queue/flush visible.

## Retry/provider

- [ ] `RetryAfterPolicy` honored.
- [ ] `max_backoff_seconds` honored.
- [ ] Retry amplification warning emitted.
- [ ] JSON mode configurable.
- [ ] Output tokens capped safely.

## Batch correctness

- [ ] Single-item invalid batches do not split.
- [ ] Repair batches do not split.
- [ ] Failed repair drops to `NeedsReview`.
- [ ] Partial batch failure cannot save as `Succeeded`.
- [ ] Batch repair maps item IDs to original block IDs.

## Performance

- [ ] Cache lookup batched and chunked.
- [ ] QA parallel.
- [ ] Prompt library cached.
- [ ] Clone pressure reduced.
- [ ] v1-fast profile exists.
- [ ] Provider presets exist.
- [ ] Windowed adaptive concurrency exists.
- [ ] Adaptive batch sizing exists.
- [ ] Compact prompts exist.

## Commands

- [ ] `bookforge doctor` exists.
- [ ] `bookforge doctor --storage` or equivalent storage section exists.
- [ ] `bookforge status <job_id>` exists.
- [ ] `bookforge tail <job_id>` exists.

## Quality gate

Run:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

If all features are unsupported:

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

---

# 13. Definition of Success

After implementation, this should be true:

```bash
bookforge doctor \
  --provider openrouter \
  --model google/gemini-2.5-flash-lite
```

prints actionable model/provider/storage recommendations.

Then:

```bash
bookforge translate book.epub \
  --target Italian \
  --provider openrouter \
  --model google/gemini-2.5-flash-lite \
  --profile v1-fast \
  --ui progress
```

shows:

- current stage;
- cache hits/misses;
- active provider requests;
- target concurrency;
- p50/p95 latency;
- 429s/timeouts/retries;
- batch split/repair activity;
- checkpoint queue/flush status;
- token throughput;
- ETA;
- final performance report.

If the run is slow, the event log should explain exactly why.

