# v1Fixes.md — Required Fixes for PR #1 Before Merge

Repository: `JunjoSick/bookforge`  
Pull request: `#1 feat: progress dashboard, storage doctor, compact prompts, and provider resilience`  
Branch: `feat/v1-progress-doctor-compact-prompts`  
Head SHA reviewed: `32c1fafeea1d2ebe4c8713e325265db82d0f4796`

Purpose: repair the current v1 implementation so it matches the roadmap/addendum and is safe to merge.

The PR implemented a large amount of the v1 roadmap, but it is not merge-ready yet. The main problems are not cosmetic. They are correctness, backpressure, retry, cancellation, and scheduler-deadlock issues.

This document is ordered by severity. Implement in order.

---

## 0. Current PR State

The PR correctly started work on:

- bounded checkpoint writer;
- WAL / busy-timeout / foreign-key SQLite setup;
- storage doctor;
- progress events and dashboard;
- lossy `ProgressSink` with `try_send`;
- compact prompts;
- retry policy fields;
- cancellation token in provider;
- JSON `response_format` auto fallback;
- chunked cache lookup;
- adaptive limiter pacing;
- batch singleton and repair-batch terminal behavior.

However, some implementation details break the intended architecture:

- async translation still uses `blocking_send`;
- batch worker queue can deadlock;
- `RetryAfterPolicy::None` causes immediate retry loops;
- `response_format` fallback fails with `provider_max_attempts = 1`;
- progress reporter is not always shut down on errors;
- `--ui json` writes to a file instead of stdout;
- `--ui auto` does not check TTY;
- JSONL flushing is missing;
- cancellation does not cover response body reads or backoff sleeps.

---

# 1. Blocker: Remove `blocking_send` From Async Translation Paths

## Problem

`CheckpointSender::send(...).await` exists and implements bounded async backpressure. But `translate.rs` still checkpoints through synchronous callbacks using:

```rust
checkpoint
    .sender
    .blocking_send(make_checkpoint_command(&checkpoint, translation))
    .map_err(LlmError::Provider)
```

This is unsafe in async runtime code.

Problems:

1. `blocking_send` can block a Tokio worker thread.
2. It may panic or behave badly when called inside async runtime contexts.
3. It bypasses the intended async backpressure mechanism.
4. It does not emit `CheckpointQueued`.
5. It does not update `queue_depth`.
6. It preserves the callback architecture that the addendum explicitly rejected.

## Files

```text
crates/bookforge-cli/src/checkpoint.rs
crates/bookforge-cli/src/commands/translate.rs
crates/bookforge-llm/src/scheduler.rs
crates/bookforge-llm/src/batch.rs
```

## Required Fix

Remove checkpointing from synchronous callbacks.

Do not use this pattern:

```rust
FnMut(&SegmentTranslation) -> Result<(), LlmError>
```

for checkpointing.

Instead:

### Non-batch mode

Pass `CheckpointSender` directly into the non-batch scheduler/worker path.

Correct flow:

```text
segment worker
  -> provider.complete(...)
  -> parse/validate
  -> finalized SegmentTranslation
  -> checkpoint_sender.send(command).await
```

A non-batch worker owns a finalized `SegmentTranslation`, so direct checkpointing is safe.

### Batch mode

Batch workers must not checkpoint raw batch results. They do not own finalized segment-level truth.

Correct flow:

```text
batch worker
  -> provider.complete(...)
  -> BatchWorkerResult
  -> result_tx

coordinator
  -> split/retry/repair
  -> aggregate item translations into SegmentTranslation
  -> validate complete block coverage
  -> mark partial/incomplete segments NeedsReview
  -> checkpoint_sender.send(finalized SegmentTranslation).await
```

## Suggested API Shape

### Non-batch scheduler

Replace or add a new function:

```rust
pub async fn translate_segments_with_checkpoint<P>(
    provider: P,
    segments: &[Segment],
    config: &TranslationRunConfig,
    checkpoint: CheckpointRuntime,
    progress: Arc<dyn ProgressSink>,
    telemetry: Arc<TelemetryLog>,
) -> Result<Vec<SegmentTranslation>, LlmError>
where
    P: LlmProvider;
```

Where:

```rust
#[derive(Clone)]
pub struct CheckpointRuntime {
    pub sender: CheckpointSender,
    pub job_id: String,
    pub provider: String,
    pub model: String,
    pub prompt_version: String,
}
```

This type can live in CLI if avoiding a `bookforge-llm` dependency on CLI checkpoint types. If `bookforge-llm` must stay independent, move checkpoint command construction into CLI and have the CLI coordinator own checkpointing.

Simplest safe approach:

1. Keep provider request logic in `bookforge-llm`.
2. Have `translate_segments_with_results(...)` return finalized `SegmentTranslation`s through a result channel.
3. CLI receives finalized translations and awaits checkpoint send.

Do not use sync callback checkpointing.

### Batch scheduler

Change batch scheduler so it returns finalized translations and/or accepts a finalization sink that is not a sync callback.

Simplest acceptable fix:

1. Keep `translate_batches_with_callback` for now, but do not call `blocking_send` in the callback.
2. Make the function return all finalized translations.
3. After it returns, loop over `fresh_translations` and call:

```rust
for translation in &fresh_translations {
    checkpoint.sender.send(make_checkpoint_command(&checkpoint, translation)).await?;
}
```

This loses per-segment checkpointing during the batch run but is safer than `blocking_send`. Better is streaming finalized translations through a bounded finalizer channel, but this is enough to remove the immediate blocker.

Preferred v1 fix:

```text
batch coordinator finalizes each SegmentTranslation
  -> checkpoint_sender.send(...).await
  -> push translation into returned Vec
```

## Delete or Restrict `blocking_send`

Either remove `CheckpointSender::blocking_send` entirely or mark it:

```rust
/// Only for non-async test/sync contexts. Do not call inside Tokio async tasks.
```

Better: remove it until a real sync use-case exists.

## Tests

Add:

```rust
#[tokio::test]
async fn non_batch_checkpoint_uses_async_send_not_blocking_send();

#[tokio::test]
async fn batch_checkpoint_happens_after_finalization();

#[tokio::test]
async fn checkpoint_send_emits_queued_event_and_updates_depth();
```

Manual grep acceptance:

```bash
rg "blocking_send" crates/bookforge-cli crates/bookforge-llm
```

Expected: no matches, or only test-only occurrences.

---

# 2. Blocker: Fix Batch Worker Queue Deadlock

## Problem

Current batch scheduler pushes all pending work into a bounded work queue before it starts collecting results:

```rust
for batch in pending.drain(..) {
    work_tx.send(batch).await;
    pushed += 1;
}

while collected < pushed {
    result_rx.recv().await;
}
```

Both queues are bounded:

```rust
work_tx capacity = concurrency * 4
result_tx capacity = concurrency * 4
```

This can deadlock:

1. Coordinator fills `work_tx`.
2. Coordinator blocks trying to push more work.
3. Workers process queued work and fill `result_tx`.
4. Workers block trying to send results.
5. Blocked workers stop draining `work_tx`.
6. Coordinator is blocked on `work_tx.send`, so it never drains `result_rx`.

## File

```text
crates/bookforge-llm/src/batch.rs
```

## Required Fix

The coordinator must interleave sending work and receiving results.

Do not push all work first.

Use a loop with `tokio::select!`.

## Implementation Shape

Maintain state:

```rust
let mut pending: VecDeque<TranslationBatch> = pending.into();
let mut in_flight = 0usize;
let mut all_results = Vec::new();
```

Coordinator loop:

```rust
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
            );
        }

        send_result = async {
            if let Some(batch) = pending.pop_front() {
                work_tx.send(batch).await.map(|_| ())
            } else {
                Ok(())
            }
        }, if !pending.is_empty() => {
            match send_result {
                Ok(()) => in_flight += 1,
                Err(_) => break,
            }
        }
    }
}
```

Alternative: use a separate feeder task. But if result handling can add split batches, the select-loop coordinator is cleaner.

## Important

When a result causes split/retry, push new batches back into `pending`:

```rust
pending.extend(split_batch(&batch));
```

Do not recursively call the scheduler.

## Tests

Add a synthetic test that previously deadlocked:

```rust
#[tokio::test]
async fn batch_scheduler_does_not_deadlock_when_work_and_result_queues_are_bounded()
```

Test setup:

- concurrency = 1 or 2;
- queue size small if configurable;
- 20+ batches;
- mock provider responds instantly;
- result channel can fill if coordinator does not drain.

Use timeout:

```rust
tokio::time::timeout(Duration::from_secs(2), run).await
```

Expected: completes.

---

# 3. Blocker: Fix `RetryAfterPolicy::None` Semantics

## Problem

Current retry delay function returns:

```rust
RetryAfterPolicy::None => None
```

But callers interpret `None` as “do not sleep, then continue retrying.”

Current shape:

```rust
let delay = retry_delay(policy, attempt, retry_after, max_backoff);
if let Some(d) = delay {
    sleep(d).await;
}
continue;
```

This means `RetryAfterPolicy::None` causes immediate retries with zero backoff.

## File

```text
crates/bookforge-llm/src/provider.rs
```

## Required Fix

`RetryAfterPolicy::None` must mean do not retry unless some other explicit config says otherwise.

Change retry handling to:

```rust
match retry_delay(policy, attempt, retry_after, max_backoff) {
    Some(delay) => {
        cancelable_sleep(&self.cancel_token, delay).await?;
    }
    None => {
        return Err(last_error.expect("set above"));
    }
}
```

Use this everywhere.

## Better Helper

Create:

```rust
async fn apply_retry_delay(
    token: &CancellationToken,
    policy: RetryAfterPolicy,
    attempt: usize,
    retry_after: Option<Duration>,
    max_backoff: Duration,
    error: LlmError,
) -> Result<()> {
    match retry_delay(policy, attempt, retry_after, max_backoff) {
        Some(delay) => {
            tokio::select! {
                _ = sleep(delay) => Ok(()),
                _ = token.cancelled() => Err(LlmError::Provider(
                    "interrupted by user".to_string()
                )),
            }
        }
        None => Err(error),
    }
}
```

Then:

```rust
apply_retry_delay(
    &self.cancel_token,
    policy,
    attempt,
    retry_after,
    max_backoff,
    last_error.expect("set above"),
).await?;
continue;
```

## Tests

Add:

```rust
#[test]
fn retry_policy_none_disables_retry_delay();

#[tokio::test]
async fn retry_policy_none_does_not_immediate_retry();

#[tokio::test]
async fn retry_policy_respect_header_sleeps_or_retries_once();
```

---

# 4. Blocker: Fix `response_format` Auto Fallback With `provider_max_attempts = 1`

## Problem

Current provider fallback handles unsupported JSON response format like this:

```rust
if status_code == 400
    && json_mode == Auto
    && use_response_format
    && !tried_response_format_fallback
{
    body.remove("response_format");
    tried_response_format_fallback = true;
    continue;
}
```

But this consumes the only loop iteration when `provider_max_attempts = 1`.

`v1-fast` should use `provider_max_attempts = 1`, so the fallback usually will not actually retry prompt-only.

## File

```text
crates/bookforge-llm/src/provider.rs
```

## Required Fix

The JSON mode fallback must not consume a normal retry attempt.

## Implementation Shape

Replace the `for attempt in 0..max_attempts` loop with a manual loop:

```rust
let mut attempt = 0usize;
let mut tried_response_format_fallback = false;

while attempt < max_attempts {
    let response = send_once(...).await;

    if unsupported_response_format(&response)
        && self.config.json_mode == JsonMode::Auto
        && use_response_format
        && !tried_response_format_fallback
    {
        self.response_format_supported.store(false, Ordering::Relaxed);
        if let Some(obj) = body.as_object_mut() {
            obj.remove("response_format");
        }
        tried_response_format_fallback = true;

        // Do not increment attempt.
        continue;
    }

    // Only count real provider attempts here.
    attempt += 1;

    ...
}
```

Alternatively, immediately retry inside the same attempt branch.

## Tests

Add:

```rust
#[tokio::test]
async fn json_mode_auto_fallback_works_with_one_provider_attempt();
```

Test behavior:

1. first provider response returns 400 when `response_format` is present;
2. provider fallback removes `response_format`;
3. second request succeeds;
4. total counted normal attempts is still accepted even when `provider_max_attempts = 1`.

---

# 5. Blocker: Always Shut Down `ProgressReporter`

## Problem

`ProgressReporter` is started before provider execution. On success, `reporter.shutdown().await` runs.

But if translation returns an error via `?`, the function returns before shutdown.

Current shape:

```rust
let reporter = ProgressReporter::spawn(...);

match config.provider.as_str() {
    ...
}?;

reporter.shutdown().await?;
```

## File

```text
crates/bookforge-cli/src/commands/translate.rs
```

## Required Fix

Use a `finalize_reporter` pattern like `finalize_writer`.

## Implementation Shape

```rust
async fn finalize_reporter<T>(
    result: Result<T, anyhow::Error>,
    reporter: ProgressReporter,
) -> Result<T> {
    let reporter_result = reporter.shutdown().await;

    match (result, reporter_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(progress_err)) => Err(progress_err),
        (Err(main_err), Ok(())) => Err(main_err),
        (Err(main_err), Err(progress_err)) => Err(anyhow::anyhow!(
            "{main_err}; additionally progress reporter failed: {progress_err}"
        )),
    }
}
```

Then:

```rust
let run_result = async {
    match config.provider.as_str() {
        "mock" => ...,
        "deepseek" | "openrouter" | "openai-compatible" => ...,
        _ => ...
    }
}.await;

finalize_reporter(run_result, reporter).await
```

## Tests

```rust
#[tokio::test]
async fn progress_reporter_shutdown_runs_after_translation_error();
```

---

# 6. High Priority: Fix `--ui json` Behavior

## Problem

The roadmap specified:

```text
--ui json => print progress events as JSON lines to stdout
```

Current `render_jsonl` writes to a file:

```rust
let path = jsonl_path.unwrap_or_else(|| PathBuf::from(".bookforge/events.jsonl"));
let mut file = std::fs::File::create(&path)?;
...
writeln!(file, "{line}")?;
```

This is not JSON UI mode. It is file logging mode.

## File

```text
crates/bookforge-cli/src/progress.rs
```

## Required Behavior

Separate UI mode from file logging.

### `--ui json`

Print every event to stdout:

```rust
println!("{line}");
```

### `--progress-jsonl path`

Also write events to that file, regardless of UI mode.

### Default event log path

If no `--progress-jsonl` is given, after `JobCreated`, write to:

```text
.bookforge/runs/<job_id>/events.jsonl
```

If this is too much for current PR, at minimum:

- `--ui json` prints to stdout;
- `--progress-jsonl` writes to file;
- no default file log yet.

## Tests

```rust
#[tokio::test]
async fn ui_json_writes_events_to_stdout();

#[tokio::test]
async fn progress_jsonl_writes_events_to_file_independent_of_ui_mode();
```

---

# 7. High Priority: Implement TTY Detection for `--ui auto`

## Problem

`UiMode::Auto` currently behaves like `UiMode::Progress`.

That means progress bars can render in CI, pipes, redirected output, and non-interactive environments.

## File

```text
crates/bookforge-cli/src/progress.rs
```

## Required Fix

Use `is-terminal`.

```rust
use std::io::IsTerminal;

let render_mode = match ui_mode {
    UiMode::Auto if std::io::stderr().is_terminal() => RenderMode::Progress,
    UiMode::Auto => RenderMode::Quiet,
    UiMode::Progress => RenderMode::Progress,
    UiMode::Json => RenderMode::JsonStdout,
    UiMode::Quiet => RenderMode::Quiet,
};
```

If `--progress-jsonl` is provided, still write JSONL in quiet mode.

## Tests

Add a pure resolver:

```rust
fn resolve_render_mode(ui_mode: UiMode, stderr_is_tty: bool) -> RenderMode
```

Tests:

```rust
#[test]
fn ui_auto_uses_progress_when_tty();

#[test]
fn ui_auto_uses_quiet_when_not_tty();
```

---

# 8. High Priority: Add JSONL Flush Policy

## Problem

JSONL writes are buffered. Current code does not flush periodically or after critical events.

On crash/interruption, the most useful recent events can be lost.

## File

```text
crates/bookforge-cli/src/progress.rs
```

## Required Fix

Flush:

- every 1–2 seconds;
- immediately after critical events.

Critical events:

```text
ProgressEvent::Error
ProgressEvent::Warning
ProgressEvent::RequestFinished where status != "ok"
ProgressEvent::BatchRepairFinished
ProgressEvent::CheckpointFlushed
ProgressEvent::TranslationFinished
ProgressEvent::DroppedEvents
```

Implementation:

```rust
fn is_critical_event(event: &ProgressEvent) -> bool {
    match event {
        ProgressEvent::Error { .. } => true,
        ProgressEvent::Warning { .. } => true,
        ProgressEvent::RequestFinished { status, .. } => status != "ok",
        ProgressEvent::BatchRepairFinished { .. } => true,
        ProgressEvent::CheckpointFlushed { .. } => true,
        ProgressEvent::TranslationFinished { .. } => true,
        ProgressEvent::DroppedEvents { .. } => true,
        _ => false,
    }
}
```

Use `BufWriter<File>`:

```rust
if is_critical_event(&event) || last_flush.elapsed() > Duration::from_secs(2) {
    writer.flush()?;
    last_flush = Instant::now();
}
```

Do not flush from worker threads. Only the reporter/audit task flushes.

## Tests

```rust
#[tokio::test]
async fn jsonl_flushes_after_critical_event();

#[tokio::test]
async fn jsonl_flushes_periodically();
```

---

# 9. High Priority: Make Cancellation Cover Body Reads and Backoff Sleeps

## Problem

The provider currently wraps only `.send()` in `tokio::select!`.

But after headers arrive, cancellation does not interrupt:

```rust
response.bytes().await
response.text().await
sleep(delay).await
```

Ctrl-C can still wait on a large response body or a long retry sleep.

## File

```text
crates/bookforge-llm/src/provider.rs
```

## Required Fix

Add helpers:

```rust
async fn cancelable<T>(
    token: &CancellationToken,
    fut: impl std::future::Future<Output = T>,
) -> Result<T> {
    tokio::select! {
        value = fut => Ok(value),
        _ = token.cancelled() => Err(LlmError::Provider(
            "interrupted by user".to_string()
        )),
    }
}

async fn cancelable_sleep(
    token: &CancellationToken,
    duration: Duration,
) -> Result<()> {
    tokio::select! {
        _ = sleep(duration) => Ok(()),
        _ = token.cancelled() => Err(LlmError::Provider(
            "interrupted by user".to_string()
        )),
    }
}
```

Use for:

```rust
client.send()
response.bytes()
response.text()
sleep(delay)
```

Example:

```rust
let response = cancelable(&self.cancel_token, send_future).await??;
let response_bytes = cancelable(&self.cancel_token, response.bytes()).await??;
```

For `response.text()`:

```rust
let response_body = cancelable(&self.cancel_token, response.text())
    .await?
    .unwrap_or_default();
```

For sleep:

```rust
cancelable_sleep(&self.cancel_token, delay).await?;
```

## Tests

```rust
#[tokio::test]
async fn cancellation_token_aborts_retry_backoff_sleep();

#[tokio::test]
async fn cancellation_token_aborts_body_read();
```

---

# 10. High Priority: Fix Storage Doctor WAL Checkpoint Call

## Problem

Current code uses:

```rust
let _ = conn.pragma_update(None, "wal_checkpoint", "PASSIVE");
```

`wal_checkpoint` is normally invoked as:

```sql
PRAGMA wal_checkpoint(PASSIVE);
```

not as an assignment-style pragma.

## File

```text
crates/bookforge-store/src/db.rs
```

## Required Fix

Replace with:

```rust
conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);")?;
```

If you want stats:

```rust
let stats = conn.query_row(
    "PRAGMA wal_checkpoint(PASSIVE)",
    [],
    |row| {
        Ok(WalCheckpointStats {
            busy: row.get(0)?,
            log: row.get(1)?,
            checkpointed: row.get(2)?,
        })
    },
);
```

For v1, `execute_batch` is sufficient.

## Tests

```rust
#[test]
fn storage_doctor_runs_passive_wal_checkpoint_without_error();
```

---

# 11. Medium Priority: Fix Progress Event Schema for Real Diagnostics

## Problem

Current `ProgressEvent::RequestFinished` is too thin:

```rust
RequestFinished {
    request_id,
    status,
    latency_ms,
    timestamp_ms,
}
```

It does not include enough information to diagnose slow runs.

## File

```text
crates/bookforge-core/src/progress.rs
```

## Required Fields

Expand request events.

```rust
RequestStarted {
    request_id: String,
    batch_id: Option<String>,
    segment_id: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    prompt_template: Option<String>,
    items: usize,
    estimated_input_tokens: usize,
    max_output_tokens: Option<u32>,
    active_requests: usize,
    target_concurrency: usize,
    timestamp_ms: u64,
}

RequestFinished {
    request_id: String,
    batch_id: Option<String>,
    segment_id: Option<String>,
    status: String,
    latency_ms: u64,
    status_code: Option<u16>,
    finish_reason: Option<String>,
    retry_count: usize,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    error_kind: Option<String>,
    timestamp_ms: u64,
}
```

## Also Expand Runtime Config

Current runtime config event is too thin. Include:

```text
provider_max_attempts
validation_max_attempts
retry_after_policy
max_backoff_seconds
json_mode
compact_prompts
thinking_disabled
model_context_tokens
max_output_tokens
batch_max_output_tokens
```

## Tests

```rust
#[test]
fn request_finished_event_serializes_diagnostic_fields();
```

---

# 12. Medium Priority: Emit Progress Events From Batch Scheduler

## Problem

The PR defines progress events but batch scheduler mostly prints via `eprintln!` and records telemetry.

Examples:

```rust
eprintln!("batch {} failed with invalid response...", batch.id);
eprintln!("repair batch {} failed...");
```

This bypasses the progress dashboard and JSONL.

## File

```text
crates/bookforge-llm/src/batch.rs
```

## Required Fix

Pass:

```rust
progress: Arc<dyn ProgressSink>
```

into batch scheduler.

Emit events for:

```text
BatchQueued
RequestStarted
RequestFinished
BatchSplit
BatchRepairStarted
BatchRepairFinished
Warning(singleton invalid batch)
Warning(repair batch terminal failure)
SegmentFinished
```

Do not use `eprintln!` for normal operational progress. Reserve `eprintln!` for fatal CLI-level messages or tracing.

## Tests

```rust
#[tokio::test]
async fn batch_scheduler_emits_split_event();

#[tokio::test]
async fn batch_scheduler_emits_repair_terminal_warning();

#[tokio::test]
async fn batch_scheduler_emits_segment_finished_after_finalization();
```

---

# 13. Medium Priority: Fix `ProgressReporter` Startup and Event Log Path

## Problem

Default JSONL path is currently:

```text
.bookforge/events.jsonl
```

Roadmap target:

```text
.bookforge/runs/<job_id>/events.jsonl
```

Also `JobCreated` currently only contains job ID, not input/output paths.

## Files

```text
crates/bookforge-core/src/progress.rs
crates/bookforge-cli/src/progress.rs
crates/bookforge-cli/src/commands/translate.rs
```

## Required Fix

Emit:

```rust
ProgressEvent::JobCreated {
    job_id: job.id.clone(),
    input_path: input.display().to_string(),
    output_path: config.output.display().to_string(),
    timestamp_ms: now_ms(),
}
```

Reporter opens default JSONL path when it receives `JobCreated`:

```text
.bookforge/runs/{job_id}/events.jsonl
```

Create parent dir.

If `--progress-jsonl` is provided, use that path instead.

## Tests

```rust
#[tokio::test]
async fn default_jsonl_path_uses_job_id_after_job_created();
```

---

# 14. Medium Priority: Fix Progress Dashboard Segment Counting

## Problem

Dashboard only increments done segments when status is `"succeeded"` or `"skipped_cached"`.

But final progress should count:

- succeeded;
- skipped_cached;
- needs_review;
- failed;

as completed units.

Otherwise a run with many needs-review segments appears stuck below 100%.

## File

```text
crates/bookforge-cli/src/progress.rs
```

## Required Fix

For `SegmentFinished`, always increment completed segments for terminal statuses:

```rust
matches!(
    status.as_str(),
    "succeeded" | "skipped_cached" | "needs_review" | "failed"
)
```

Track separate counters:

```rust
succeeded
cached
needs_review
failed
```

## Tests

```rust
#[test]
fn progress_counts_needs_review_and_failed_as_completed();
```

---

# 15. Medium Priority: Fix `seg/min` Label

Current dashboard says:

```rust
let rate = done_segments as f64 / elapsed;
rate_bar.set_message(format!("{rate:.1} seg/min"));
```

But `elapsed` is seconds, so this is segments per second mislabeled as per minute.

## File

```text
crates/bookforge-cli/src/progress.rs
```

## Fix

```rust
let rate_per_min = done_segments as f64 / elapsed * 60.0;
```

---

# 16. Medium Priority: Provider Doctor Should Use Cancel Token and JSON Fallback Semantics

## Problem

`doctor` uses `OpenAiCompatibleProvider::new(...)`, not `new_with_cancel(...)`, and does not test JSON fallback behavior robustly.

## File

```text
crates/bookforge-cli/src/commands/doctor.rs
```

## Fix

Use cancellation token where available, or local token:

```rust
let cancel_token = CancellationToken::new();
let provider = OpenAiCompatibleProvider::new_with_cancel(config, cancel_token)?;
```

Test both:

- JSON with response_format;
- prompt-only fallback if first attempt fails with unsupported response_format.

For now, this is lower priority than fixing provider fallback itself.

---

# 17. Medium Priority: Remove `/dev/null` Workaround Entirely

The PR body mentions a `/dev/null` issue in `progress.rs`, but the current fetched progress file did not show `/dev/null`. Still ensure no platform-specific null-device path remains.

Run:

```bash
rg "/dev/null|NUL" crates/bookforge-cli/src/progress.rs crates
```

If needed, use:

```rust
#[cfg(unix)]
PathBuf::from("/dev/null")

#[cfg(windows)]
PathBuf::from("NUL")
```

Better: do not open a null file at all. Use `None`.

---

# 18. Medium Priority: Add Missing `v1-fast` / Provider Presets If Not Present

PR summary says progress/provider resilience were added, but check whether `TranslationProfile::V1Fast` and `ProviderPreset` exist.

If missing, add them.

## Files

```text
crates/bookforge-core/src/config.rs
crates/bookforge-cli/src/commands/translate.rs
```

## `v1-fast` Defaults

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

## Provider Presets

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

Apply preset before explicit CLI overrides.

---

# 19. Test Bundle Required Before Merge

Add or update these tests before merging.

## Checkpoint / backpressure

```rust
#[tokio::test]
async fn checkpoint_send_emits_queued_event_and_updates_depth();

#[tokio::test]
async fn non_batch_checkpoint_uses_async_send_not_blocking_send();

#[tokio::test]
async fn batch_checkpoint_happens_after_finalization();
```

## Batch queue

```rust
#[tokio::test]
async fn batch_scheduler_does_not_deadlock_when_work_and_result_queues_are_bounded();
```

## Provider retry/fallback/cancellation

```rust
#[tokio::test]
async fn retry_policy_none_does_not_immediate_retry();

#[tokio::test]
async fn json_mode_auto_fallback_works_with_one_provider_attempt();

#[tokio::test]
async fn cancellation_token_aborts_retry_backoff_sleep();

#[tokio::test]
async fn cancellation_token_aborts_body_read();
```

## Progress reporter

```rust
#[tokio::test]
async fn progress_reporter_shutdown_runs_after_translation_error();

#[tokio::test]
async fn ui_json_writes_events_to_stdout();

#[test]
fn ui_auto_uses_progress_when_tty();

#[test]
fn ui_auto_uses_quiet_when_not_tty();

#[tokio::test]
async fn jsonl_flushes_after_critical_event();
```

## Storage doctor

```rust
#[test]
fn storage_doctor_runs_passive_wal_checkpoint_without_error();
```

## Existing correctness tests to preserve

```rust
#[tokio::test]
async fn single_item_invalid_batch_does_not_split_forever();

#[tokio::test]
async fn repair_batch_invalid_json_does_not_split();

#[tokio::test]
async fn partial_batch_failure_without_successful_repair_marks_segment_needs_review();

#[test]
fn batched_cache_lookup_chunks_over_sqlite_parameter_limit();
```

---

# 20. Required Manual Checks

Run:

```bash
cargo fmt --all
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
rg "format!\(\"\{:\?\}\", settings\.profile\)" crates
rg "/dev/null" crates
rg "unbounded_channel" crates/bookforge-cli/src/checkpoint.rs
```

Expected:

- no `blocking_send` in production translation paths;
- no profile namespace based on `Debug`;
- no platform-specific `/dev/null`;
- no unbounded checkpoint channel.

---

# 21. Implementation Order

Use this exact order.

```text
1. Remove blocking_send from async translation paths.
2. Make checkpointing use async CheckpointSender::send.
3. Ensure batch checkpointing happens only after final SegmentTranslation finalization.
4. Fix bounded batch queue deadlock by interleaving enqueue/result collection.
5. Fix RetryAfterPolicy::None semantics.
6. Fix response_format fallback with provider_max_attempts = 1.
7. Ensure ProgressReporter shutdown on all errors.
8. Fix --ui json stdout behavior.
9. Implement --ui auto TTY detection.
10. Add JSONL flush policy.
11. Make body-read/text-read/backoff-sleep cancellation-aware.
12. Fix storage doctor wal_checkpoint call.
13. Expand request progress event schema.
14. Emit batch progress events instead of eprintln for operational events.
15. Fix dashboard completed-count and seg/min math.
16. Add missing v1-fast/provider presets if absent.
17. Run full fmt/clippy/test suite.
```

---

# 22. Acceptance Criteria

Do not merge until all are true.

## Checkpointing

- [ ] No production translation path calls `CheckpointSender::blocking_send`.
- [ ] Async checkpoint send is used where checkpointing happens during async execution.
- [ ] Non-batch workers checkpoint only finalized segment translations.
- [ ] Batch scheduler checkpoints only after split/repair/aggregation/completeness validation.
- [ ] Checkpoint queued events and queue depth work for actual translation paths.

## Batch scheduler

- [ ] Bounded work/result queues cannot deadlock.
- [ ] Split/retry/repair behavior still works.
- [ ] Singleton invalid batch remains terminal.
- [ ] Repair batch failure remains terminal and marks unresolved items `NeedsReview`.

## Provider

- [ ] `RetryAfterPolicy::None` does not cause immediate retry loops.
- [ ] JSON response_format fallback works when provider attempts = 1.
- [ ] Cancellation interrupts send, body read, response text read, and backoff sleep.

## Progress

- [ ] Reporter shuts down on both success and error.
- [ ] `--ui json` prints JSON lines to stdout.
- [ ] `--progress-jsonl` writes file logs independently of UI mode.
- [ ] `--ui auto` checks TTY.
- [ ] JSONL flushes periodically and after critical events.
- [ ] Dashboard counts `needs_review` and `failed` as completed terminal states.
- [ ] Rate label is correctly segments/minute.

## Storage

- [ ] WAL / busy-timeout / FK remain enabled.
- [ ] Storage doctor uses valid `PRAGMA wal_checkpoint(PASSIVE);`.
- [ ] WAL sidecars are explained, not deleted.

## Quality

- [ ] `cargo fmt --all` passes.
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings` passes, or equivalent if all-features unsupported.
- [ ] `cargo test --workspace --all-features` passes, or equivalent if all-features unsupported.

---

# 23. Summary for Codex

This PR implemented the right broad idea but still has unsafe execution details.

The biggest conceptual fix:

> Checkpointing is durable backpressure and must use async `CheckpointSender::send`. Progress is lossy UI and must use non-blocking `try_send`.

Do not confuse the two.

The biggest mechanical fix:

> Remove `blocking_send` and do not push all bounded batch work before draining bounded results.

Once those are fixed, the PR will be much closer to v1-ready.
