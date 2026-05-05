# v1Final.md — BookForge Final V1 Implementation Plan

Repository: `JunjoSick/bookforge`  
Audience: Codex / implementation agent  
Scope: final v1 implementation, beyond the current progress-dashboard PR  
Status assumption: PR #1 has been run locally with `cargo fmt`, `cargo check`, `cargo clippy`, and `cargo test` passing.

This document is intentionally detailed. It is meant to be handed to an implementation agent that may not have all prior context.

---

## 0. Executive Summary

BookForge is an AI-assisted EPUB/book translation tool. It has moved from “experimental CLI script” toward a real product through PR #1:

- progress event system;
- terminal dashboard;
- JSONL event logging;
- provider doctor and storage doctor;
- bounded checkpoint writer;
- WAL SQLite hardening;
- compact prompts;
- provider retry/backoff/cancellation improvements;
- batch repair and truncation handling;
- chunked cache lookup;
- adaptive limiter improvements.

The remaining goal is to finish **true v1**.

True v1 means:

1. **A user can run a full book translation and always see what is happening.**
2. **The tool is fast with non-thinking models.**
3. **The tool can explain why it is slow when it is slow.**
4. **The tool checkpoints finalized progress during execution, not only after completion.**
5. **A crash/interruption does not waste already finalized work.**
6. **Provider errors, SQLite errors, batch split/repair issues, and cache misses are visible and actionable.**
7. **The CLI feels like a product, not a black box.**

The most important final architectural upgrade is:

> Stream finalized `SegmentTranslation` objects to the checkpoint writer during execution, without reintroducing async callback lifetime traps or checkpointing raw batch results.

---

# 1. Non-Negotiable Principles

## 1.1 Durable work and visual progress are different

There are two separate data flows:

```text
Durable checkpoint flow:
  bounded
  backpressured
  not lossy
  may slow translation if SQLite falls behind

Progress/UI flow:
  bounded
  non-blocking
  lossy
  must never slow translation
```

Do not confuse them.

Correct:

```rust
checkpoint_sender.send(cmd).await?; // durable, backpressured
progress.emit(event);               // non-blocking try_send internally
```

Incorrect:

```rust
progress_tx.send(event).await;       // can make terminal lag pause translation
checkpoint_tx.try_send(cmd);         // can lose durable work
```

---

## 1.2 Batch workers must not checkpoint raw batch results

Non-batch workers translate one segment, so they can directly produce finalized `SegmentTranslation`.

Batch workers translate a batch item group. They produce `BatchTranslationResult`, not final segment-level truth.

Batch checkpointing must happen after:

- split/retry decisions;
- repair attempts;
- batch item aggregation;
- original block-ID restoration;
- complete block coverage validation;
- final `Succeeded` / `NeedsReview` / `Failed` status decision.

Correct:

```text
batch worker
  -> BatchWorkerResult
  -> coordinator
  -> repair/split/aggregate/finalize
  -> finalized SegmentTranslation
  -> checkpoint_sender.send(...)
```

Incorrect:

```text
batch worker
  -> BatchTranslationResult
  -> checkpoint writer
```

---

## 1.3 Avoid Rust async callback lifetime traps

Do not implement checkpoint backpressure with generic async callbacks like:

```rust
F: FnMut(SegmentTranslation) -> Fut,
Fut: Future<Output = Result<(), LlmError>>
```

That pattern creates avoidable lifetime/HRTB complexity.

Prefer concrete runtime handles:

```rust
CheckpointSender
ProgressSink
CancellationToken
mpsc::Sender<FinalizedSegment>
```

---

## 1.4 UI events are best-effort

The progress event bus must use `try_send`.

If the terminal is slow, SSH is laggy, or stdout is blocked, BookForge must continue translating.

Dropped progress events should be counted and reported.

Important data for final reports should come from:

- telemetry structures;
- DB state;
- checkpoint writer;
- final summaries;

not only from lossy UI events.

---

## 1.5 SQLite sidecar files are normal

With WAL enabled, SQLite may create:

```text
jobs.sqlite-wal
jobs.sqlite-shm
```

Doctor/status should explain these are normal if integrity checks pass.

Do not delete WAL sidecars automatically.

---

# 2. Current V1 Baseline After PR #1

Assume PR #1 has landed or is about to land with:

- `ProgressEvent`;
- `ProgressSink`;
- `ChannelProgressSink`;
- `ProgressReporter`;
- `UiMode::{Auto, Progress, Json, Quiet}`;
- `--progress-jsonl`;
- central JSONL file writer;
- `JsonlFileWriter`;
- progress tests for quiet/json/progress JSONL modes;
- `CheckpointWriter`;
- bounded `CheckpointSender`;
- SQLite WAL/busy-timeout/FK setup;
- storage doctor;
- provider doctor;
- retry policy fields;
- cancellation-aware provider requests;
- `BatchKind::{Translation, Repair}`;
- singleton invalid-batch circuit breaker;
- repair-batch terminal behavior;
- `BatchSizer`;
- `v1-fast` / provider presets if already added;
- compact batch prompts;
- chunked cache lookup.

The final implementation should **not** redo all of that from scratch. It should harden and complete it.

---

# 3. Final V1 Target User Experience

## 3.1 Primary command

```bash
bookforge translate book.epub \
  --target Italian \
  --provider openrouter \
  --model google/gemini-2.5-flash-lite \
  --profile v1-fast \
  --provider-preset openrouter-paid-fast \
  --ui progress
```

## 3.2 Expected live dashboard

```text
BookForge v1 — translating book.epub → Italian

Stage              Translation
Input              book.epub
Output             book.it.epub
Job                job_20260505_abc123

Segmentation       428 segments / 1,982 blocks / ~612k source tokens
Cache              116 reused, 312 pending
Translation        ███████████████░░░░░░  241/312 pending segments 77.2%
Batches            46/61 done | active 12 | split 3 | repair 2
Provider           active 18 | target 24 | p50 2.3s | p95 8.8s
Provider errors    429s 4 | timeouts 1 | invalid JSON 2 | truncations 1
Tokens             input 412k | output 186k | 12.9k output tok/min
Checkpoint         queued 0 | flushed 241 | last flush 180ms
Throughput         38.4 blocks/min | ETA 00:07:31
Last event         batch_0042 ok, 37 items, 2.1s, 1,942 output tokens
```

## 3.3 Expected final summary

```text
Done.

Input:  book.epub
Output: book.it.epub
Job:    job_20260505_abc123

Segments:
  total:        428
  cached:       116
  succeeded:    306
  needs review: 6
  failed:       0

Provider:
  requests:      61
  p50 latency:   2.3s
  p95 latency:   8.8s
  429s:          4
  timeouts:      1
  invalid JSON:  2
  truncations:   1

Tokens:
  input:  412,000
  output: 186,000

Performance:
  elapsed:            14m 22s
  blocks/min:         38.4
  output tokens/min:  12,946

Artifacts:
  EPUB:   book.it.epub
  Report: .bookforge/reports/job_20260505_abc123.md
  Events: .bookforge/runs/job_20260505_abc123/events.jsonl
```

## 3.4 Required diagnostic commands

```bash
bookforge doctor --provider openrouter --model google/gemini-2.5-flash-lite
bookforge doctor --storage
bookforge status <job_id>
bookforge tail <job_id>
bookforge resume <job_id>
```

---

# 4. Final Architecture Overview

## 4.1 Translation pipeline

```text
translate command
  -> resolve config/profile/preset
  -> start ProgressReporter
  -> open JobStore
  -> create/resume job
  -> emit JobCreated
  -> read EPUB
  -> build IR
  -> segment
  -> compute cache namespace
  -> batch cache lookup
  -> pending segments
  -> start CheckpointWriter
  -> execute translation mode:
       non-batch
       batch
  -> stream finalized checkpoints during execution
  -> flush CheckpointWriter
  -> QA / fallback / double-check if configured
  -> rebuild EPUB
  -> validate
  -> write report
  -> mark job done
  -> emit TranslationFinished
  -> shutdown ProgressReporter
```

## 4.2 Non-batch execution

```text
pending segments
  -> bounded worker queue
  -> segment worker:
       acquire permit
       request provider
       validate response
       create finalized SegmentTranslation
       send checkpoint
       emit SegmentFinished
       send result to coordinator
  -> coordinator collects finalized translations
```

## 4.3 Batch execution

```text
translation batches
  -> bounded batch work queue
  -> batch workers:
       acquire permit
       request provider
       parse batch response
       send BatchWorkerResult to coordinator

  -> coordinator:
       receive result
       split invalid multi-item batches
       terminalize singleton invalid batches
       terminalize repair batch failures
       retry transient translation batches
       collect successful/failure item results
       run repair pass if configured
       aggregate item results by segment
       validate block coverage
       finalized SegmentTranslation
       checkpoint finalized translation
       emit SegmentFinished
       return finalized translations
```

## 4.4 Durable checkpoint path

```text
finalized SegmentTranslation
  -> CheckpointCommand::SaveTranslation
  -> CheckpointSender::send(...).await
  -> bounded mpsc channel
  -> spawn_blocking SQLite writer
  -> JobStore::save_translation / save_needs_review / mark_failed
  -> CheckpointFlushed event
```

## 4.5 Progress path

```text
ProgressEvent
  -> ProgressSink::emit
  -> try_send bounded channel
  -> if full, increment dropped counter
  -> ProgressReporter
       -> optional JSONL writer
       -> terminal renderer / stdout JSON / quiet
```

---

# 5. Final Implementation Phases

Use this order.

```text
Phase A: Streaming checkpoint finalization
Phase B: Batch coordinator hardening
Phase C: Resume/interruption semantics
Phase D: Progress/report completeness
Phase E: Provider/model performance tuning
Phase F: Final tests and quality gates
```

---

# Phase A — Streaming Checkpoint Finalization

This is the most important remaining true-v1 upgrade.

## A1. Current limitation

If checkpointing happens only after the scheduler returns, a crash halfway through a long run loses all completed in-memory translations since no checkpoint commands were sent yet.

V1 needs checkpoints as finalized segments become available.

## A2. Non-batch streaming checkpoints

### Target flow

```text
worker produces finalized SegmentTranslation
  -> checkpoint_sender.send(...).await
  -> result_tx.send(translation).await
```

### Files

```text
crates/bookforge-llm/src/scheduler.rs
crates/bookforge-cli/src/commands/translate.rs
crates/bookforge-cli/src/checkpoint.rs
```

### Preferred API

Keep `bookforge-llm` independent of CLI checkpoint types if possible.

Option 1: LLM scheduler streams finalized translations through a result channel. CLI owns checkpointing.

```rust
pub async fn translate_segments_streaming<P>(
    provider: P,
    segments: &[Segment],
    config: &TranslationRunConfig,
    telemetry: Arc<TelemetryLog>,
    progress: Arc<dyn ProgressSink>,
    output_tx: mpsc::Sender<SegmentTranslation>,
    cancel_token: CancellationToken,
) -> Result<(), LlmError>
where
    P: LlmProvider;
```

CLI:

```rust
let (finalized_tx, mut finalized_rx) =
    mpsc::channel::<SegmentTranslation>(settings.scheduler.concurrency * 2);

let translate_task = tokio::spawn(translate_segments_streaming(
    provider,
    &pending_segments,
    &run_config,
    telemetry.clone(),
    progress.clone(),
    finalized_tx,
    cancel_token.clone(),
));

let mut fresh_translations = Vec::new();

while let Some(translation) = finalized_rx.recv().await {
    checkpoint_sender
        .send(make_checkpoint_command(&checkpoint, &translation))
        .await?;
    progress.emit(segment_finished_event(&translation));
    fresh_translations.push(translation);
}

translate_task.await??;
```

Option 2: define an async trait-like sink. Avoid unless necessary.

Recommendation: use channel streaming, not async trait/callback.

### Non-batch worker shape

```rust
loop {
    let permit = limiter.acquire().await?;

    let Some(segment) = work_rx.recv().await else {
        drop(permit);
        break;
    };

    let translation = translate_one(...).await?;

    output_tx.send(translation).await.map_err(|_| {
        LlmError::Provider("finalized segment channel closed".to_string())
    })?;

    drop(permit);
}
```

### Acceptance

- A long non-batch run checkpoints segment 1 before segment N finishes.
- Killing the process after some segments finish preserves those checkpointed segments.
- No `blocking_send`.
- No async closure HRTB pattern.

---

## A3. Batch streaming checkpoints

### Target flow

Batch streaming is coordinator-driven.

```text
batch workers -> BatchWorkerResult -> coordinator -> finalized SegmentTranslation -> output_tx
```

The CLI receives `SegmentTranslation` from `output_tx` and sends it to checkpoint writer.

### Files

```text
crates/bookforge-llm/src/batch.rs
crates/bookforge-cli/src/commands/translate.rs
```

### Preferred API

```rust
pub async fn translate_batches_streaming<P>(
    provider: P,
    batches: Vec<TranslationBatch>,
    segments: &[Segment],
    config: &TranslationRunConfig,
    telemetry: Arc<TelemetryLog>,
    limiter: Option<Arc<AdaptiveLimiter>>,
    batch_sizer: Option<&mut BatchSizer>,
    progress: Arc<dyn ProgressSink>,
    output_tx: mpsc::Sender<SegmentTranslation>,
    cancel_token: CancellationToken,
) -> Result<Vec<SegmentTranslation>, LlmError>
where
    P: LlmProvider;
```

The function may still return `Vec<SegmentTranslation>` for final aggregation, but it must also send each finalized translation as soon as it is finalized.

### Coordinator finalization

When the coordinator has a finalized segment:

```rust
let translation = finalize_segment(...);

output_tx.send(translation.clone()).await.map_err(|_| {
    LlmError::Provider("finalized segment channel closed".to_string())
})?;

finalized.push(translation);
```

### Important

Do not send partial segment translations before repair/completeness validation.

If the segment is incomplete after repair:

```rust
translation.status = SegmentStatus::NeedsReview;
translation.error = Some("batch translation missing block translations: ...".to_string());
```

Then send it.

### Acceptance

- Batch mode checkpoints finalized segments during execution.
- Raw `BatchTranslationResult` is never checkpointed.
- Failed repair results are checkpointed as `NeedsReview`.
- Progress increments during batch run, not only after batch scheduler returns.

---

## A4. Checkpoint command creation remains in CLI

Prefer not to make `bookforge-llm` depend on `bookforge-cli`.

Use finalized segment channel:

```text
bookforge-llm produces SegmentTranslation
CLI converts SegmentTranslation -> CheckpointCommand
CLI sends to CheckpointSender
```

This keeps crate boundaries cleaner.

---

## A5. Streaming checkpoint tests

Add:

```rust
#[tokio::test]
async fn non_batch_streams_finalized_segments_before_completion();

#[tokio::test]
async fn batch_streams_finalized_segments_after_aggregation();

#[tokio::test]
async fn batch_never_streams_raw_batch_results();

#[tokio::test]
async fn interrupted_run_preserves_already_streamed_checkpoints();
```

The interruption test can simulate cancellation after the first N finalized segments.

---

# Phase B — Batch Coordinator Hardening

## B1. Bounded queue non-deadlock regression

The batch scheduler must not deadlock when both work and result channels are bounded.

Add or keep:

```rust
#[tokio::test]
async fn batch_scheduler_does_not_deadlock_when_work_and_result_queues_are_bounded();
```

Test shape:

```rust
let run = async {
    let batches = many_small_batches(100);
    let provider = fast_mock_provider();
    translate_batches_streaming(...).await.unwrap();
};

tokio::time::timeout(Duration::from_secs(2), run)
    .await
    .expect("batch scheduler should not deadlock");
```

Conditions:

- low concurrency, such as 1 or 2;
- enough batches to exceed work queue capacity;
- fast mock provider so result queue pressure is possible;
- bounded work and result queues.

---

## B2. Prefer select-based coordinator

Current try-send plus drain-one-result approach may be acceptable, but a `select!` coordinator is clearer.

Recommended structure:

```rust
let mut pending: VecDeque<TranslationBatch> = batches.into();
let mut in_flight = 0usize;
let mut all_results = Vec::new();

while !pending.is_empty() || in_flight > 0 {
    tokio::select! {
        biased;

        maybe_result = result_rx.recv(), if in_flight > 0 => {
            let Some((batch, result)) = maybe_result else {
                break;
            };

            in_flight -= 1;

            handle_batch_result(
                batch,
                result,
                &mut pending,
                &mut all_results,
                ...
            ).await?;
        }

        send_result = async {
            let Some(batch) = pending.pop_front() else {
                return Ok(());
            };
            work_tx.send(batch).await
        }, if !pending.is_empty() => {
            match send_result {
                Ok(()) => in_flight += 1,
                Err(_) => break,
            }
        }
    }
}
```

If using `try_send`, keep the regression test.

---

## B3. Repair batch terminal behavior

Keep:

```rust
BatchKind::Repair
```

Rules:

```text
Translation batch + invalid JSON + len > 1:
  split

Translation batch + invalid JSON + len == 1:
  terminal NeedsReview

Repair batch + invalid JSON:
  terminal NeedsReview
  no split
  no recursive repair
```

Add/keep tests:

```rust
#[tokio::test]
async fn repair_batch_invalid_json_does_not_split();

#[tokio::test]
async fn repair_batch_failure_marks_items_needs_review();

#[tokio::test]
async fn repair_batch_is_not_repaired_recursively();

#[tokio::test]
async fn singleton_invalid_batch_does_not_split_forever();
```

---

## B4. Batch request status mapping

Replace generic `"error"` with specific statuses.

Helper:

```rust
fn request_status_from_error(error: &LlmError) -> &'static str {
    match error {
        LlmError::HttpStatus { status: 429, .. } => "rate_limited",
        LlmError::HttpStatus { status, .. } if *status >= 500 => "server_error",
        LlmError::Http(error) if error.is_timeout() => "timeout",
        LlmError::Http(error) if error.is_connect() => "connect_error",
        LlmError::InvalidResponse(msg) if msg.contains("truncated") => "truncated",
        LlmError::InvalidResponse(_) => "invalid_response",
        LlmError::Json(_) => "json_error",
        _ => "error",
    }
}
```

Use in `ProgressEvent::RequestFinished` and telemetry.

---

## B5. Replace batch `eprintln!` with progress events

Operational progress should go through `ProgressSink`, not raw `eprintln!`.

Replace messages like:

```rust
eprintln!("batch {} failed with invalid response, splitting", batch.id);
```

with:

```rust
progress.emit(ProgressEvent::BatchSplit {
    batch_id: batch.id.clone(),
    old_items: batch.items.len(),
    left_items,
    right_items,
    reason: "invalid_response".to_string(),
    timestamp_ms: now_ms(),
});
```

For repair terminal failure:

```rust
progress.emit(ProgressEvent::Warning {
    kind: "repair_batch_failed".to_string(),
    message: format!(
        "repair batch {} failed; marking {} items NeedsReview",
        batch.id,
        batch.items.len()
    ),
    timestamp_ms: now_ms(),
});
```

Reserve `eprintln!` for fatal CLI-level messages outside progress rendering.

---

## B6. BatchSizer progress event

If `BatchSizer` changes batch sizes, emit:

```rust
ProgressEvent::BatchSizingChanged {
    batch_id: None,
    previous_target,
    new_target,
    previous_max_items,
    new_max_items,
    reason,
    timestamp_ms,
}
```

No raw `eprintln!`.

---

# Phase C — Resume and Interruption Semantics

## C1. Goal

If a user hits Ctrl-C, BookForge should preserve already finalized/checkpointed work and resume cleanly.

## C2. Cancellation flow

```text
Ctrl-C
  -> cancel token
  -> stop queueing new work
  -> in-flight provider futures are cancelled through tokio::select!
  -> finalized result channel drains
  -> checkpoint writer flushes
  -> job status becomes interrupted
  -> progress reporter clears
  -> print resume command
```

## C3. Provider cancellation

Ensure cancellation wraps:

- HTTP send;
- response body read;
- response text read;
- retry sleep;
- QA requests;
- double-check requests;
- provider doctor request if appropriate.

Helper:

```rust
async fn cancelable<T>(
    token: &CancellationToken,
    fut: impl Future<Output = T>,
) -> Result<T, LlmError> {
    tokio::select! {
        value = fut => Ok(value),
        _ = token.cancelled() => Err(LlmError::Provider(
            "interrupted by user".to_string()
        )),
    }
}
```

## C4. Mark job interrupted

Add store method if absent:

```rust
pub fn mark_job_interrupted(&self, job_id: &str) -> Result<()>;
```

Should set:

```sql
jobs.status = 'interrupted'
jobs.updated_at = CURRENT_TIMESTAMP
```

## C5. Resume command

If not already implemented, add:

```bash
bookforge resume <job_id>
```

Minimal behavior:

1. load job config;
2. reload input EPUB;
3. rebuild segments with same settings or stored namespace;
4. use cache/checkpoint state to skip completed segments;
5. translate pending/needs-review as configured;
6. rebuild output.

If full resume is too large, `status` should at least print the exact command needed to resume with `translate --resume-job <id>`.

---

# Phase D — Progress, JSONL, and Reports

## D1. JSONL writer behavior

Current target:

```text
--ui controls rendering
--progress-jsonl controls file logging
```

Already implemented centrally. Preserve this.

Required behavior:

```text
--ui quiet --progress-jsonl file:
  no bars, no stdout JSON, file written

--ui json --progress-jsonl file:
  stdout JSON and file written

--ui progress --progress-jsonl file:
  bars and file written

--ui auto --progress-jsonl file:
  progress if TTY, quiet if non-TTY, file written either way
```

## D2. Default JSONL path

Final v1 should write default JSONL path even when the user does not pass `--progress-jsonl`.

Target:

```text
.bookforge/runs/<job_id>/events.jsonl
```

Implementation:

```rust
if self.path.is_none()
    && self.writer.is_none()
    && let ProgressEvent::JobCreated { job_id, .. } = event
{
    self.path = Some(PathBuf::from(".bookforge/runs")
        .join(job_id)
        .join("events.jsonl"));
}
```

This appears implemented. Preserve it.

## D3. JSONL flushing

Flush:

- every 2 seconds;
- on important events:
  - error;
  - warning;
  - non-ok request finish;
  - batch repair finished;
  - checkpoint flushed;
  - translation finished;
  - dropped events.

Keep:

```rust
fn is_important_event(event: &ProgressEvent) -> bool
```

## D4. Dropped event reporting

At reporter shutdown, if dropped count > 0:

```text
(137 progress events dropped)
```

Also consider emitting:

```rust
ProgressEvent::DroppedEvents { count, timestamp_ms }
```

if the reporter can generate internal events.

## D5. Progress dashboard counts

Terminal statuses count as completed:

```text
succeeded
skipped_cached
needs_review
failed
```

Do not leave progress below 100% because some segments need review.

## D6. Final report

Report must include:

```markdown
## Performance

| Metric | Value |
|---|---:|
| Elapsed | ... |
| Requests | ... |
| p50 latency | ... |
| p95 latency | ... |
| 429s | ... |
| Timeouts | ... |
| Invalid JSON | ... |
| Truncations | ... |
| Input tokens | ... |
| Output tokens | ... |
| Blocks/min | ... |
| Output tokens/min | ... |
```

Data sources:

- `TelemetryLog`;
- final progress state if reliable;
- DB summary;
- checkpoint counts;
- provider metrics.

Do not rely solely on lossy UI events.

---

# Phase E — Provider and Performance Tuning

## E1. Provider retry policy

Preserve corrected semantics:

```text
RetryAfterPolicy::None:
  do not retry / return current error

RespectHeader:
  use Retry-After if present, otherwise configured fallback

Fixed:
  fixed delay capped by max_backoff

JitteredExponential:
  exponential delay with jitter, capped by max_backoff
```

Tests:

```rust
retry_policy_none_does_not_immediate_retry
retry_policy_respect_header_uses_retry_after
retry_policy_caps_to_max_backoff
```

## E2. JSON response format fallback

Preserve corrected behavior:

- if `response_format` rejected with unsupported 400;
- and `JsonMode::Auto`;
- remove `response_format`;
- retry prompt-only;
- do not consume normal provider attempt budget.

Test:

```rust
json_mode_auto_fallback_works_with_one_provider_attempt
```

## E3. Output token cap

Ensure every provider request computes:

```rust
cap_output_tokens(
    computed,
    estimated_prompt_tokens,
    model_context_tokens,
    user_cap,
)
```

Avoid:

```text
prompt_tokens + max_output_tokens > model_context_tokens
```

Emit warning if capped substantially.

## E4. HTTP client

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

Do not force HTTP/1 unless a provider-specific bug requires it.

## E5. Adaptive concurrency

Final behavior:

```text
429:
  halve target concurrency immediately

timeout/connect:
  reduce to 75%

p95 too high:
  reduce 10–20%

stable success window:
  grow by +1 every few seconds

never grow on every single success
```

Use monotonic `Instant` for internal timing, not wall-clock `SystemTime`.

## E6. Adaptive batch sizing

Final behavior:

```text
truncation:
  target_tokens *= 0.65
  max_items *= 0.75

invalid JSON:
  target_tokens *= 0.75
  max_items *= 0.85

high p95:
  target_tokens *= 0.85

stable success:
  target_tokens *= 1.10
```

Clamp:

```text
Plain/Turbo:   4k..32k
MarkerSafe:    2k..16k
RunPreserving: 1k..8k
```

Current implementation uses a general clamp. For true v1, mode-specific clamps are better.

---

# Phase F — Configuration and Presets

## F1. `v1-fast` profile

Must exist.

Defaults:

```text
batch enabled: true
batch target tokens: 16_000
batch max items: 128
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

## F2. Provider presets

Must exist:

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

Apply preset before explicit CLI overrides.

Suggested preset defaults:

```text
OpenRouterFree:
  concurrency 1–2
  provider attempts 1
  validation attempts 1
  batch target 4k–8k
  max items 32–64
  retry_after_policy RespectHeader

OpenRouterPaidFast:
  concurrency 16–32
  provider attempts 1
  validation attempts 1
  batch target 16k
  max items 128

GeminiFlashLite:
  concurrency 32–64
  provider attempts 1
  validation attempts 1
  batch target 16k–24k
  max items 128–250

DeepSeekFree:
  concurrency 1
  provider attempts 1
  validation attempts 1
  batch target 4k

DeepSeekPaid:
  concurrency 4–16
  provider attempts 2
  validation attempts 1
  batch target 8k–16k
```

Do not infer paid/free tier automatically.

---

# Phase G — Cache, Prompt, Marker, and DB Correctness

## G1. Chunked cache lookup

Use chunk size:

```rust
const SQLITE_IN_CHUNK_SIZE: usize = 900;
```

Never query a whole book with one huge `IN (...)`.

## G2. Cache compatibility

A cache hit is valid only if:

- cache namespace matches;
- prompt version matches;
- provider/model matches;
- language matches;
- segment block IDs match exactly;
- returned block translations are ordered according to current `segment.block_ids`.

## G3. Stable identifiers

Do not use:

```rust
format!("{:?}", settings.profile)
```

inside cache namespace.

Use:

```rust
TranslationProfile::namespace_str()
PromptVersion::as_str()
```

## G4. Marker parsing

Marker parsing should live in core:

```text
crates/bookforge-core/src/marker.rs
```

Functions:

```rust
extract_marker_id
is_marker_token
marker_ids_in_text
```

Use same logic in EPUB writer and LLM validation.

---

# Phase H — CLI Commands

## H1. `doctor`

Subcommands or flags:

```bash
bookforge doctor --provider openrouter --model ...
bookforge doctor --storage
```

Provider doctor checks:

- API key env var;
- base URL;
- tiny provider request;
- latency;
- JSON response_format support;
- usage token support;
- reasoning detection;
- suggested profile/preset.

Storage doctor checks:

- DB exists;
- WAL sidecars;
- `PRAGMA integrity_check`;
- `PRAGMA journal_mode`;
- `PRAGMA wal_checkpoint(PASSIVE);`;
- busy timeout / FK behavior if feasible.

## H2. `status`

```bash
bookforge status <job_id>
```

Show:

```text
Job
Status
Input
Output
Source/target language
Provider/model
Segments total/succeeded/cached/needs_review/failed/pending
Tokens
Last update
Report path
Events path
Resume command
```

## H3. `tail`

```bash
bookforge tail <job_id>
```

Reads:

```text
.bookforge/runs/<job_id>/events.jsonl
```

Displays recent events or reconstructs progress dashboard.

## H4. `resume`

Preferred final command:

```bash
bookforge resume <job_id>
```

If full resume is not yet implemented, `status` should tell user exactly how to resume manually.

---

# Phase I — Testing Plan

## I1. Quality gate

Always run:

```bash
cargo fmt --all
cargo check --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

If `--all-features` is unsupported:

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Also run grep checks:

```bash
rg "blocking_send" crates/bookforge-cli crates/bookforge-llm
rg "unbounded_channel" crates/bookforge-cli/src/checkpoint.rs
rg "/dev/null" crates
rg "format!\(\"\\{:\\?\\}\", settings\\.profile\)" crates
```

Expected:

- no `blocking_send` in production translation paths;
- no unbounded checkpoint channel;
- no uncfg-gated `/dev/null`;
- no Debug-format profile namespace in cache keys.

---

## I2. Streaming checkpoint tests

```rust
#[tokio::test]
async fn non_batch_streams_finalized_segments_before_completion();

#[tokio::test]
async fn batch_streams_finalized_segments_after_aggregation();

#[tokio::test]
async fn batch_never_streams_raw_batch_results();

#[tokio::test]
async fn interrupted_run_preserves_already_streamed_checkpoints();
```

---

## I3. Batch scheduler tests

```rust
#[tokio::test]
async fn batch_scheduler_does_not_deadlock_when_work_and_result_queues_are_bounded();

#[tokio::test]
async fn single_item_invalid_batch_does_not_split_forever();

#[tokio::test]
async fn repair_batch_invalid_json_does_not_split();

#[tokio::test]
async fn repair_batch_failure_marks_items_needs_review();

#[tokio::test]
async fn repair_batch_is_not_repaired_recursively();

#[tokio::test]
async fn partial_batch_failure_without_successful_repair_marks_segment_needs_review();
```

---

## I4. Progress tests

```rust
#[test]
fn ui_auto_uses_progress_when_tty();

#[test]
fn ui_auto_uses_quiet_when_not_tty();

#[tokio::test]
async fn progress_jsonl_writes_file_in_quiet_mode();

#[tokio::test]
async fn progress_jsonl_writes_file_in_json_stdout_mode();

#[tokio::test]
async fn progress_jsonl_writes_file_in_progress_mode();

#[tokio::test]
async fn progress_sink_drops_events_when_channel_full_instead_of_blocking();

#[test]
fn important_events_are_flushed();
```

---

## I5. Provider tests

```rust
#[tokio::test]
async fn retry_policy_none_does_not_immediate_retry();

#[tokio::test]
async fn json_mode_auto_fallback_works_with_one_provider_attempt();

#[tokio::test]
async fn cancellation_token_aborts_retry_backoff_sleep();

#[tokio::test]
async fn cancellation_token_aborts_body_read();

#[test]
fn output_token_budget_respects_model_context_window();

#[test]
fn output_token_budget_respects_user_cap();
```

---

## I6. Storage tests

```rust
#[test]
fn job_store_enables_wal_and_busy_timeout();

#[test]
fn job_store_enables_foreign_keys_on_every_connection();

#[test]
fn storage_doctor_runs_passive_wal_checkpoint_without_error();

#[test]
fn doctor_reports_wal_sidecars_as_normal();

#[test]
fn batched_cache_lookup_chunks_over_sqlite_parameter_limit();

#[test]
fn batched_cache_lookup_orders_blocks_by_current_segment_order();
```

---

# Phase J — Manual Smoke Tests

## J1. Mock provider progress mode

```bash
cargo run -- translate fixtures/sample.epub \
  --target Italian \
  --provider mock \
  --model mock-prefix-target \
  --profile v1-fast \
  --ui progress \
  --progress-jsonl /tmp/bookforge-events.jsonl
```

Expected:

- dashboard appears;
- JSONL file written;
- output EPUB created;
- report created.

## J2. JSON stdout mode

```bash
cargo run -- translate fixtures/sample.epub \
  --target Italian \
  --provider mock \
  --model mock-prefix-target \
  --ui json \
  --progress-jsonl /tmp/bookforge-events-json-mode.jsonl \
  > /tmp/bookforge-stdout.jsonl
```

Expected:

- stdout contains JSON events;
- file also contains JSON events;
- no progress bar control characters.

## J3. Quiet mode with JSONL

```bash
cargo run -- translate fixtures/sample.epub \
  --target Italian \
  --provider mock \
  --model mock-prefix-target \
  --ui quiet \
  --progress-jsonl /tmp/bookforge-events-quiet.jsonl
```

Expected:

- minimal stdout/stderr;
- JSONL file written.

## J4. Provider doctor

```bash
cargo run -- doctor \
  --provider openrouter \
  --model google/gemini-2.5-flash-lite
```

Expected:

- API key status;
- latency;
- JSON mode status;
- recommended preset.

## J5. Storage doctor

```bash
cargo run -- doctor --storage
```

Expected:

- journal mode;
- integrity check;
- WAL sidecar explanation.

## J6. Ctrl-C test

Start a long mock/slow-provider translation, hit Ctrl-C.

Expected:

- progress bars clear;
- job marked interrupted;
- checkpoint writer flushes;
- resume/status command shown;
- already finalized segments preserved.

---

# Phase K — Final V1 Acceptance Checklist

## K1. Core correctness

- [ ] Batch truncation returns an error and triggers split logic.
- [ ] Singleton invalid batch is terminal.
- [ ] Repair batch is terminal and never recursively repaired.
- [ ] Batch repair maps item IDs to original block IDs.
- [ ] Partial/incomplete batch segments become `NeedsReview`.
- [ ] Cache namespace is stable and uses typed prompt/profile identifiers.
- [ ] Cache block order matches current segment order.
- [ ] Marker parsing is centralized.

## K2. Persistence

- [ ] WAL enabled.
- [ ] Busy timeout enabled.
- [ ] Foreign keys enabled per connection.
- [ ] Checkpoint channel bounded.
- [ ] Checkpoint writer surfaces original DB errors.
- [ ] Finalized segments are checkpointed during execution.
- [ ] Ctrl-C flushes checkpoint writer.
- [ ] Resume/status can identify completed vs pending work.

## K3. Progress and UI

- [ ] `--ui progress` dashboard works.
- [ ] `--ui json` writes JSON to stdout.
- [ ] `--ui quiet` suppresses UI noise.
- [ ] `--ui auto` uses TTY detection.
- [ ] `--progress-jsonl` writes file in every UI mode.
- [ ] Default JSONL path is job-based.
- [ ] JSONL flushes important events.
- [ ] Progress events are lossy and non-blocking.
- [ ] Dropped event count visible.
- [ ] Dashboard counts failed/needs-review as completed terminal states.
- [ ] ETA and throughput visible.

## K4. Provider behavior

- [ ] Retry policy config is actually honored.
- [ ] `RetryAfterPolicy::None` does not immediate-retry.
- [ ] JSON response-format fallback works with one provider attempt.
- [ ] Cancellation interrupts send/body/text/sleep.
- [ ] Output tokens capped by context/user settings.
- [ ] HTTP client pooling enabled.
- [ ] Doctor can detect provider issues.

## K5. Speed

- [ ] `v1-fast` profile exists.
- [ ] Provider presets exist.
- [ ] Compact prompts selected when configured.
- [ ] QA runs concurrently.
- [ ] Batch cache lookup is chunked and non-N+1.
- [ ] Adaptive concurrency uses windowed growth/reduction.
- [ ] Adaptive batch sizing exists and emits events.
- [ ] Performance report shows latency and throughput.

## K6. Commands

- [ ] `translate`
- [ ] `doctor`
- [ ] `doctor --storage`
- [ ] `status`
- [ ] `tail`
- [ ] `resume` or documented resume equivalent

---

# 6. Recommended Final Implementation Order

Use this exact order for remaining true-v1 work.

```text
1. Merge PR #1 if it passes local/CI checks and GitHub mergeability is fixed.
2. Add streaming finalized segment checkpointing for non-batch mode.
3. Add streaming finalized segment checkpointing for batch mode.
4. Add interruption/resume persistence test.
5. Harden batch coordinator with deadlock regression tests.
6. Replace remaining batch eprintln! operational messages with progress events.
7. Improve request status mapping in batch telemetry/progress.
8. Ensure default job-based JSONL path is stable and documented.
9. Add or complete resume command.
10. Finalize performance report fields.
11. Run all quality gates and smoke tests.
```

---

# 7. Known Acceptable Intermediate Limitation

If PR #1 is merged before streaming checkpointing is implemented, record this explicitly:

```md
Known limitation:
This PR avoids unsafe `blocking_send` and uses bounded checkpointing, but translation results are checkpointed after scheduler completion rather than continuously during execution. A crash/interruption before scheduler completion can lose in-memory finalized results. Follow-up v1 work will stream finalized `SegmentTranslation` objects to the checkpoint writer during execution.
```

This is acceptable only as an intermediate merge, not final v1.

---

# 8. Final Definition of Done

BookForge v1 is done when a user can:

```bash
bookforge doctor --provider openrouter --model google/gemini-2.5-flash-lite
```

get actionable provider/storage recommendations, then run:

```bash
bookforge translate book.epub \
  --target Italian \
  --provider openrouter \
  --model google/gemini-2.5-flash-lite \
  --profile v1-fast \
  --provider-preset openrouter-paid-fast \
  --ui progress
```

and see:

- live progress;
- accurate ETA;
- cache hit rate;
- provider latency and errors;
- batch split/repair events;
- checkpoint flush status;
- token throughput;
- final performance report;
- durable partial progress after interruption.

If it is slow, the event log and report must explain why.

If it is interrupted, already finalized work must not be lost.

If SQLite sidecars appear, doctor must explain them.

If the model/provider misbehaves, doctor/progress/report must make that visible.

That is v1.
