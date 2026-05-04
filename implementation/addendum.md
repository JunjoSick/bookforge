# addendum.md — BookForge v1 Execution & Architecture Guidelines

Target: Codex / implementation agent  
Repository: `JunjoSick/bookforge`  
Purpose: patch the v1 roadmap with Rust/Tokio/SQLite/reqwest/indicatif execution details that are easy to miss from a high-level plan.

This addendum is **not** a replacement for `planForV1.md`. It is a guardrail document. Follow it while implementing the roadmap.

---

## 0. Core Rule

Do not make correctness depend on UI, terminal rendering, or generic async callback magic.

Use concrete runtime handles:

- `CheckpointSender` for durable checkpoint persistence;
- `ProgressSink` for lossy progress/UI events;
- `CancellationToken` for cancellation;
- worker/result channels for scheduler coordination.

---

# 1. Async Closures vs Direct Channels

## Trap

The v1 plan originally suggested making checkpoint callbacks async:

```rust
F: FnMut(SegmentTranslation) -> Fut,
Fut: Future<Output = Result<(), LlmError>>
```

This is a Rust trap. Async closures that capture references frequently trigger difficult lifetime and HRTB errors such as:

```text
implementation of `FnMut` is not general enough
```

or future-outlives-borrow errors.

## Directive

Do **not** implement checkpoint backpressure through generic async callbacks.

Abandon the checkpoint callback pattern.

Instead, pass concrete cloned handles directly into the schedulers/workers:

```rust
checkpoint_tx: CheckpointSender
progress: Arc<dyn ProgressSink>
cancel_token: CancellationToken
```

Then workers/coordinators call:

```rust
checkpoint_tx.send(command).await?;
progress.emit(event); // non-blocking inside ProgressSink
```

This integrates naturally with Tokio backpressure and avoids closure lifetime hell.

---

# 2. Worker vs Coordinator Responsibilities

This distinction is important.

## Non-batch mode

In non-batch mode, each worker translates exactly one segment and owns a finalized `SegmentTranslation`.

Therefore non-batch workers may checkpoint directly:

```text
segment worker
  -> provider.complete(...)
  -> parse/validate
  -> final SegmentTranslation
  -> checkpoint_tx.send(...).await
```

This is correct because the worker has complete segment-level truth.

## Batch mode

In batch mode, network workers do **not** own finalized segment-level truth.

A batch worker produces a `BatchTranslationResult`, not a final `SegmentTranslation`.

The coordinator still has to:

- split invalid multi-item batches;
- stop splitting terminal singleton failures;
- decide transient retry behavior;
- run or skip repair;
- prevent repair batches from splitting;
- merge multiple item translations into one segment translation;
- restore original block IDs;
- verify complete block coverage;
- downgrade partial segments to `NeedsReview`;
- preserve final segment ordering/status.

Therefore batch workers must **not** checkpoint raw batch results.

Correct batch architecture:

```text
batch workers
  -> BatchWorkerResult through result_tx
    -> coordinator
      -> split / retry / repair / aggregate / completeness-check
        -> finalized SegmentTranslation
          -> checkpoint_tx.send(...).await
```

If the coordinator must remain non-blocking on checkpoint backpressure, add a finalizer task:

```text
batch workers
  -> result_tx
    -> coordinator finalizes SegmentTranslation
      -> finalized_tx bounded channel
        -> checkpoint finalizer task
          -> checkpoint_tx.send(...).await
            -> SQLite writer
```

The critical rule:

> Workers should checkpoint directly only when they own a finalized `SegmentTranslation`.

For batch mode, finalization happens after coordination, not inside raw provider workers.

---

# 3. Bounded Checkpoint Channel

## Trap

An unbounded checkpoint channel can grow indefinitely if LLM requests complete faster than SQLite commits.

## Directive

Use a bounded channel:

```rust
const CHECKPOINT_QUEUE_CAPACITY: usize = 64;

let (tx, rx) = tokio::sync::mpsc::channel::<CheckpointCommand>(
    CHECKPOINT_QUEUE_CAPACITY,
);
```

Expose a concrete sender wrapper:

```rust
#[derive(Clone)]
pub struct CheckpointSender {
    tx: tokio::sync::mpsc::Sender<CheckpointCommand>,
    queue_depth: Arc<AtomicUsize>,
    progress: Arc<dyn ProgressSink>,
}
```

`CheckpointSender::send` should await and therefore apply real backpressure:

```rust
impl CheckpointSender {
    pub async fn send(&self, cmd: CheckpointCommand) -> Result<(), LlmError> {
        let queued = self.queue_depth.fetch_add(1, Ordering::AcqRel) + 1;

        match self.tx.send(cmd).await {
            Ok(()) => {
                self.progress.emit(ProgressEvent::CheckpointQueued {
                    queued,
                    timestamp_ms: now_ms(),
                });
                Ok(())
            }
            Err(err) => {
                self.queue_depth.fetch_sub(1, Ordering::AcqRel);
                Err(LlmError::Provider(format!(
                    "checkpoint queue closed; checkpoint writer may have failed: {err}"
                )))
            }
        }
    }
}
```

The writer decrements `queue_depth` when it receives a command.

Checkpointing is not UI. It should not be lossy. If SQLite falls behind, translation should slow down rather than accumulate infinite memory.

---

# 4. Progress UI Events Must Be Lossy

## Trap

If progress events use:

```rust
progress_tx.send(event).await
```

then a slow terminal renderer, SSH session, or JSONL writer can pause LLM API requests and disk writes.

## Directive

`ProgressSink::emit` must be synchronous and non-blocking:

```rust
pub trait ProgressSink: Send + Sync + 'static {
    fn emit(&self, event: ProgressEvent);
}
```

The channel implementation must use `try_send`:

```rust
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

Recommended capacity:

```rust
const PROGRESS_EVENT_QUEUE_CAPACITY: usize = 2048;
```

## Important distinction

There are two kinds of events:

```text
Progress/UI events:
  lossy, non-blocking, try_send, best-effort.

Critical telemetry:
  must survive enough for final reports/debugging, via TelemetryLog/DB/report path.
```

If JSONL must preserve critical events more reliably, do **not** make the UI channel blocking. Instead add a separate audit sink:

```text
ProgressSink:
  lossy UI events.

AuditSink:
  bounded important-event channel.
  flushes every 1–2 seconds.
  flushes immediately after Error/Warning/RateLimited/BatchRepairFinished.
```

For v1, JSONL may be best-effort diagnostics as long as final telemetry/report data remains reliable.

---

# 5. JSONL Durability

## Trap

If JSONL uses `BufWriter`, events sit in memory until the buffer fills. On crash or `SIGKILL`, the most recent diagnostic events can be lost.

## Directive

For JSONL event logging:

- flush every 1–2 seconds with a background ticker;
- flush immediately after critical events:
  - `Error`;
  - `Warning`;
  - `RequestFinished { status: RateLimited | Timeout | InvalidResponse | Truncated }`;
  - `BatchRepairFinished`;
  - `CheckpointFlushed` if debugging persistence;
  - shutdown/interruption events.

Do not block hot workers on JSONL flushing. Flush in the reporter/audit task.

---

# 6. SQLite WAL Mode and Sidecar Files

## Trap

`PRAGMA journal_mode=WAL` is necessary for concurrent writer/reader behavior, but it creates sidecar files:

```text
jobs.sqlite-wal
jobs.sqlite-shm
```

If BookForge crashes or is force-killed, these files may remain. Users may mistake them for corruption.

## Directive

`JobStore::open` must enable on every connection:

```rust
conn.busy_timeout(Duration::from_secs(5))?;
conn.pragma_update(None, "journal_mode", "WAL")?;
conn.pragma_update(None, "synchronous", "NORMAL")?;
conn.pragma_update(None, "foreign_keys", "ON")?;
```

`bookforge doctor --storage` must:

1. detect `.sqlite`, `.sqlite-wal`, and `.sqlite-shm`;
2. open the DB;
3. run:

```sql
PRAGMA integrity_check;
PRAGMA journal_mode;
PRAGMA wal_checkpoint(PASSIVE);
```

4. explicitly tell the user that WAL sidecars are normal if integrity passes.

Example output:

```text
SQLite storage:
  database: .bookforge/jobs.sqlite
  journal mode: wal
  sidecars: jobs.sqlite-wal, jobs.sqlite-shm present
  integrity_check: ok

Note:
  WAL sidecar files are normal. SQLite will recover them automatically.
  Do not delete them manually while BookForge is running.
```

Do not delete sidecars automatically.

---

# 7. SQLite `IN (...)` Parameter Limits

## Trap

SQLite limits host parameters in one statement. Depending on build flags, the limit may be 999. A whole-book batched cache lookup can exceed this.

## Directive

Batched cache lookup must chunk:

```rust
const SQLITE_IN_CHUNK_SIZE: usize = 900;

for chunk in segments.chunks(SQLITE_IN_CHUNK_SIZE) {
    // SELECT ... WHERE source_hash IN (?, ?, ...)
}
```

900 leaves room for other bind parameters.

Do not use a single `IN` clause over every segment in the book.

Required tests:

```rust
#[test]
fn batched_cache_lookup_chunks_over_sqlite_parameter_limit()
```

---

# 8. Adaptive Limiter and Worker Queue Ordering

## Trap

If a worker pulls work from `work_rx` first and then waits for an adaptive permit, it can trap batches inside suspended workers.

Bad:

```rust
let batch = work_rx.recv().await;
let permit = limiter.acquire().await;
```

If the limiter throttles down to 2 while 64 workers have already pulled work, many batches are stuck in idle workers and unavailable to active workers.

## Directive

Acquire an adaptive permit before pulling work.

Correct shape:

```rust
loop {
    let permit = limiter.acquire().await?;

    let Some(batch) = work_rx.recv().await else {
        drop(permit);
        break;
    };

    active.fetch_add(1, Ordering::AcqRel);

    let result = process_batch(batch).await;

    active.fetch_sub(1, Ordering::AcqRel);
    drop(permit);

    result_tx.send(result).await?;
}
```

## Caveat

Do not count a worker as active until it has both:

1. a permit;
2. a work item.

If it acquires a permit and finds the work channel closed, it must immediately drop the permit.

This pattern is okay:

```text
64 worker tasks spawned
adaptive limiter target = 2
62 workers wait on permits
2 workers process work
```

Idle tasks are cheap. Trapped work is the thing to avoid.

---

# 9. Cancellation and TCP Connections

## Trap

`CancellationToken` does not automatically interrupt a running future. If a worker is awaiting:

```rust
provider.complete(request).await
```

and the provider hangs, Ctrl-C may not take effect until the HTTP timeout expires.

## Directive

Wrap provider calls in `tokio::select!`.

```rust
tokio::select! {
    result = provider.complete(request) => result,
    _ = cancel_token.cancelled() => {
        Err(LlmError::Provider("interrupted by user".to_string()))
    }
}
```

Dropping the `reqwest` future cancels the in-flight request and drops the TCP operation.

Use this in:

- non-batch segment requests;
- batch translation requests;
- repair requests;
- QA requests;
- double-check requests;
- provider doctor test requests if cancellation is wired globally.

On cancellation:

1. stop queueing new work;
2. allow checkpoint writer to flush queued finalized translations;
3. mark job `interrupted`;
4. print resume instructions.

---

# 10. Terminal State and Panic Hooks

## Trap

`indicatif` can hide the terminal cursor and rewrite lines. A panic or abrupt exit can leave the terminal in a broken visual state.

## Directive

When using progress UI:

1. install a panic hook;
2. clear progress bars before printing panic details;
3. explicitly restore terminal visibility if needed;
4. clear the `MultiProgress` on graceful Ctrl-C before printing resume instructions.

Sketch:

```rust
let previous_hook = std::panic::take_hook();
std::panic::set_hook(Box::new(move |panic_info| {
    progress_cleanup_handle.clear();
    eprintln!();
    previous_hook(panic_info);
}));
```

On graceful interruption:

```rust
multi.clear()?;
println!("Interrupted.");
println!("Saved progress: ...");
println!("Resume: bookforge resume {job_id}");
```

Do not print the interrupted status while progress bars are still active.

---

# 11. Repair Batches Are Terminal

## Trap

Normal batches can split. Repair batches must not.

If a repair batch returns invalid JSON and enters normal split logic, it can cause infinite split/repair cascades.

## Directive

Add batch kind:

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

Coordinator error logic:

```rust
match (batch.kind, result) {
    (BatchKind::Repair, Err(error)) => {
        // Do not split.
        // Do not retry through normal batch cascade.
        // Mark unresolved repair items NeedsReview.
    }

    (BatchKind::Translation, Err(LlmError::InvalidResponse(_)))
        if batch.items.len() > 1 =>
    {
        // split
    }

    (BatchKind::Translation, Err(LlmError::InvalidResponse(_)))
        if batch.items.len() == 1 =>
    {
        // terminal singleton failure -> NeedsReview
    }

    ...
}
```

If repair fails:

```text
repair failed -> unresolved items/segments become NeedsReview
```

Do not recursively repair repair failures.

Required tests:

```rust
#[tokio::test]
async fn repair_batch_invalid_json_does_not_split()

#[tokio::test]
async fn repair_batch_failure_marks_items_needs_review()

#[tokio::test]
async fn repair_batch_is_not_repaired_recursively()
```

---

# 12. Singleton Batch Circuit Breaker

## Trap

If an invalid batch keeps splitting, eventually it reaches one item. A one-item batch cannot split further.

## Directive

Explicitly handle:

```rust
Err(LlmError::InvalidResponse(_)) if batch.items.len() == 1
```

as terminal.

Do **not** call `split_batch` on singleton invalid batches.

Mark the item/segment as `NeedsReview`.

Required test:

```rust
#[tokio::test]
async fn single_item_invalid_batch_does_not_split_forever()
```

---

# 13. Batch Checkpointing: Finalization Before Persistence

This is the refined correction to the earlier addendum.

## Rule

Never checkpoint raw batch-worker results.

Batch checkpointing must happen after:

- all relevant splits are resolved;
- repair has been attempted or skipped;
- failed repair items are marked `NeedsReview`;
- item translations are aggregated by segment;
- original block IDs are restored;
- complete block coverage is checked.

Only then create `CheckpointCommand::SaveTranslation`.

Correct batch flow:

```text
BatchWorkerResult
  -> coordinator
    -> maybe split
    -> maybe retry
    -> maybe repair
    -> aggregate to SegmentTranslation
    -> completeness validation
    -> finalized SegmentTranslation
    -> checkpoint_tx.send(...).await
```

Non-batch flow can checkpoint directly because worker output is already segment-final.

---

# 14. Output Token Budget Limits

## Trap

Large computed `max_tokens` can cause provider 400s or slow routing if:

```text
prompt_tokens + max_tokens > model_context_window
```

## Directive

Add optional runtime caps:

```rust
model_context_tokens: Option<u32>
max_output_tokens: Option<u32>
batch_max_output_tokens: Option<u32>
```

Budget function:

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

Emit a warning when capped substantially:

```text
max_output_tokens reduced from 16384 to 7424 to fit model context
```

---

# 15. Request and Retry Observability

## Directive

Every provider request should produce enough metadata for diagnosis:

- request ID;
- batch ID or segment ID;
- provider;
- model;
- prompt template;
- estimated input tokens;
- max output tokens;
- latency;
- status code;
- finish reason;
- retry count;
- backoff;
- request status:
  - `Ok`;
  - `RateLimited`;
  - `Timeout`;
  - `InvalidResponse`;
  - `Truncated`;
  - `Failed`;
  - `Cancelled`.

Retry policy must honor:

```rust
RetryAfterPolicy
max_backoff_seconds
provider_max_attempts
```

The runtime config event must show these values.

---

# 16. Implementation Order Patch

Apply these addendum constraints in this order:

```text
1. Replace checkpoint callback plan with concrete CheckpointSender plumbing.
2. Split checkpointing responsibilities:
   - non-batch worker direct checkpoint;
   - batch coordinator/finalizer checkpoint only after finalization.
3. Make checkpoint channel bounded.
4. Make ProgressSink non-blocking/lossy with try_send.
5. Add WAL/busy-timeout/FK pragmas and doctor --storage WAL sidecar messaging.
6. Add SQLite IN chunking to batched cache lookup.
7. Add BatchKind::{Translation, Repair}.
8. Add singleton invalid-batch circuit breaker.
9. Add repair-batch terminal failure handling.
10. Wrap provider calls in tokio::select! for cancellation.
11. Add terminal cleanup/panic hook for indicatif.
12. Add JSONL flush policy for critical events.
```

---

# 17. Acceptance Checklist

## Checkpointing

- [ ] No generic async callback is used for checkpoint backpressure.
- [ ] `CheckpointSender` is passed directly into translation execution.
- [ ] Non-batch workers checkpoint directly only after final `SegmentTranslation`.
- [ ] Batch workers do not checkpoint raw batch results.
- [ ] Batch coordinator/finalizer checkpoints only finalized `SegmentTranslation`.
- [ ] Checkpoint channel is bounded.
- [ ] Checkpoint send applies backpressure.
- [ ] Checkpoint writer errors remain surfaced.

## Progress

- [ ] `ProgressSink::emit` is synchronous.
- [ ] Channel progress sink uses `try_send`.
- [ ] Full UI channel drops events rather than blocking workers.
- [ ] Dropped progress event count is tracked.
- [ ] JSONL flushes every 1–2 seconds or after critical events.
- [ ] Critical telemetry is not solely dependent on lossy progress events.

## SQLite

- [ ] WAL enabled on every connection.
- [ ] Busy timeout set on every connection.
- [ ] Foreign keys enabled on every connection.
- [ ] Doctor explains `-wal` and `-shm` sidecars.
- [ ] Doctor runs `PRAGMA integrity_check`.
- [ ] Batched cache lookup chunks by at most 900 segment hashes.

## Batch

- [ ] `BatchKind` exists.
- [ ] Translation batches may split.
- [ ] Repair batches do not split.
- [ ] Singleton invalid translation batches do not split.
- [ ] Failed repair items become `NeedsReview`.
- [ ] Repair failures do not recursively repair.
- [ ] Batch checkpointing happens only after segment finalization.

## Cancellation and terminal

- [ ] Provider calls are wrapped in `tokio::select!`.
- [ ] Ctrl-C cancels in-flight reqwest futures.
- [ ] Progress bars clear on Ctrl-C.
- [ ] Panic hook clears progress UI before printing panic trace.

---

# 18. Minimal Tests Required by This Addendum

```rust
#[tokio::test]
async fn checkpoint_channel_applies_backpressure();

#[tokio::test]
async fn non_batch_worker_checkpoints_final_segment_translation();

#[tokio::test]
async fn batch_worker_does_not_checkpoint_raw_batch_result();

#[tokio::test]
async fn batch_coordinator_checkpoints_only_after_completeness_validation();

#[tokio::test]
async fn progress_sink_drops_events_when_channel_full_instead_of_blocking();

#[test]
fn sqlite_cache_lookup_chunks_to_avoid_parameter_limit();

#[tokio::test]
async fn repair_batch_invalid_json_does_not_split();

#[tokio::test]
async fn singleton_invalid_batch_does_not_split_forever();

#[tokio::test]
async fn cancellation_token_aborts_in_flight_provider_request();

#[test]
fn doctor_storage_reports_wal_sidecars_as_normal();
```

---

# 19. Final Warning

The most dangerous implementation mistake would be to combine these two ideas incorrectly:

```text
bounded checkpoint channel
+
generic async callback
```

That creates Rust lifetime pain.

The clean solution is:

```text
concrete CheckpointSender
+
direct worker/coordinator ownership
+
bounded channel
```

Use direct handles, not callback magic.
