# v1Fixes2.md — Final Fixes Before Merging PR #1

Repository: `JunjoSick/bookforge`  
Pull request: `#1 feat: progress dashboard, storage doctor, compact prompts, and provider resilience`  
Branch: `feat/v1-progress-doctor-compact-prompts`  
Reviewed head SHA: `2cc681aba2fe98c8464055e25513f40474355022`

Purpose: fix the remaining merge blockers after the second review pass.

The previous major blockers are mostly addressed:

- `blocking_send` is gone from production translation paths.
- `ProgressReporter` is now finalized even when translation errors.
- `RetryAfterPolicy::None` no longer creates zero-delay retry storms.
- JSON `response_format` fallback no longer consumes the normal provider attempt budget.
- Provider cancellation now covers `.send()`, response body reads, response text reads, and retry sleeps.
- `PRAGMA wal_checkpoint(PASSIVE);` is now called correctly.
- `--ui auto` now checks whether stderr is a TTY.
- Progress event schema is expanded enough for current diagnostics.

There are still a few things to fix before merging.

---

## 0. Merge Decision Summary

Do **not** merge until at least these are fixed:

```text
1. Fix likely compile error in progress.rs around `succeeded + c`.
2. Make `--progress-jsonl` write events independently of UI mode.
3. Run cargo fmt / clippy / test and confirm green.
```

Then decide whether to merge with this known limitation:

```text
Checkpointing currently happens after the scheduler returns, not continuously as each segment finalizes.
```

That limitation is safe enough for an intermediate PR if explicitly accepted, but it is not full v1 behavior. For true v1 readiness, checkpoint finalized segment translations during execution.

---

# 1. Blocker: Fix `progress.rs` Compile Error in `TranslationFinished`

## Problem

In `crates/bookforge-cli/src/progress.rs`, this arm likely does not compile:

```rust
ProgressEvent::TranslationFinished {
    succeeded,
    cached: c,
    needs_review,
    failed,
    ..
} => {
    seg_bar.set_position(*succeeded as u64 + *c as u64);
    seg_bar.finish_with_message(format!(
        "{} done, {} needs review, {} failed",
        succeeded + c,
        needs_review,
        failed
    ));
}
```

Because the match is over `&event`, `succeeded`, `c`, `needs_review`, and `failed` are references.

This expression is wrong:

```rust
succeeded + c
```

## File

```text
crates/bookforge-cli/src/progress.rs
```

## Required Fix

Dereference values before arithmetic/formatting.

Suggested replacement:

```rust
ProgressEvent::TranslationFinished {
    succeeded,
    cached: c,
    needs_review,
    failed,
    ..
} => {
    let done = *succeeded + *c;

    seg_bar.set_position(done as u64);
    seg_bar.finish_with_message(format!(
        "{done} done, {} needs review, {} failed",
        *needs_review,
        *failed
    ));

    stage_bar.finish_and_clear();
    batch_bar.finish_and_clear();
    rate_bar.finish_and_clear();
    checkpoint_bar.finish_and_clear();
}
```

## Acceptance

Run:

```bash
cargo check --workspace
```

This should pass before continuing.

---

# 2. Blocker: Make `--progress-jsonl` Independent of UI Mode

## Problem

Current `render_loop(...)` routes by UI mode:

```rust
match effective_mode {
    UiMode::Quiet => while rx.recv().await.is_some() {},
    UiMode::Json => {
        render_jsonl_stdout(&mut rx).await?;
    }
    UiMode::Progress | UiMode::Auto => {
        render_progress_bars(&mut rx, jsonl_path, &dropped).await?;
    }
}
```

That means:

```bash
bookforge translate ... --ui quiet --progress-jsonl events.jsonl
```

does **not** write `events.jsonl`.

And:

```bash
bookforge translate ... --ui json --progress-jsonl events.jsonl
```

prints JSON to stdout but does **not** write the JSONL file.

File writing currently exists only inside `render_progress_bars(...)`.

This violates the intended behavior:

```text
--ui controls terminal/stdout rendering.
--progress-jsonl controls file logging.
They must be independent.
```

## File

```text
crates/bookforge-cli/src/progress.rs
```

## Required Fix

Create a central event loop that always handles file logging first, then dispatches rendering behavior.

Do **not** put JSONL writing only inside `render_progress_bars`.

---

## 2.1 Suggested Architecture

Add a render mode enum:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenderMode {
    Quiet,
    Progress,
    JsonStdout,
}
```

Resolve mode:

```rust
fn resolve_render_mode(ui_mode: UiMode, stderr_is_tty: bool) -> RenderMode {
    match ui_mode {
        UiMode::Auto if stderr_is_tty => RenderMode::Progress,
        UiMode::Auto => RenderMode::Quiet,
        UiMode::Progress => RenderMode::Progress,
        UiMode::Json => RenderMode::JsonStdout,
        UiMode::Quiet => RenderMode::Quiet,
    }
}
```

Then `render_loop` should own both:

1. optional JSONL file writer;
2. selected UI renderer.

Shape:

```rust
async fn render_loop(
    mut rx: mpsc::Receiver<ProgressEvent>,
    ui_mode: UiMode,
    jsonl_path: Option<PathBuf>,
    dropped: Arc<AtomicUsize>,
) -> Result<()> {
    let render_mode = resolve_render_mode(ui_mode, std::io::stderr().is_terminal());

    let mut file_writer = JsonlFileWriter::new(jsonl_path);
    let mut renderer = Renderer::new(render_mode, dropped)?;

    while let Some(event) = rx.recv().await {
        file_writer.write_event(&event)?;
        renderer.handle_event(&event)?;
    }

    file_writer.flush()?;
    renderer.finish()?;

    Ok(())
}
```

This makes file logging independent of UI mode.

---

## 2.2 JSONL Writer Type

Add:

```rust
struct JsonlFileWriter {
    path: Option<PathBuf>,
    writer: Option<BufWriter<std::fs::File>>,
    failed: bool,
    last_flush: Instant,
}
```

Implementation:

```rust
impl JsonlFileWriter {
    fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            writer: None,
            failed: false,
            last_flush: Instant::now(),
        }
    }

    fn ensure_open(&mut self) -> Result<()> {
        if self.writer.is_some() || self.failed {
            return Ok(());
        }

        let Some(path) = self.path.as_ref() else {
            return Ok(());
        };

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        match std::fs::File::create(path) {
            Ok(file) => {
                self.writer = Some(BufWriter::new(file));
                self.last_flush = Instant::now();
            }
            Err(err) => {
                self.failed = true;
                eprintln!("warn: cannot create JSONL progress log {}: {err}", path.display());
            }
        }

        Ok(())
    }

    fn write_event(&mut self, event: &ProgressEvent) -> Result<()> {
        self.ensure_open()?;

        let Some(writer) = self.writer.as_mut() else {
            return Ok(());
        };

        writeln!(writer, "{}", serde_json::to_string(event)?)?;

        if is_important_event(event)
            || self.last_flush.elapsed() >= std::time::Duration::from_secs(2)
        {
            writer.flush()?;
            self.last_flush = Instant::now();
        }

        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if let Some(writer) = self.writer.as_mut() {
            writer.flush()?;
        }
        Ok(())
    }
}
```

If you want JSONL to be enabled by default later, open the default path when `JobCreated` arrives. For this PR, it is enough that explicit `--progress-jsonl` works in all UI modes.

---

## 2.3 Renderer Type

Keep current progress bar logic, but move state into a renderer object so it can be driven from the shared loop.

Minimal structure:

```rust
enum Renderer {
    Quiet,
    JsonStdout,
    Progress(ProgressBars),
}
```

Methods:

```rust
impl Renderer {
    fn new(mode: RenderMode, dropped: Arc<AtomicUsize>) -> Result<Self>;

    fn handle_event(&mut self, event: &ProgressEvent) -> Result<()> {
        match self {
            Renderer::Quiet => Ok(()),
            Renderer::JsonStdout => {
                println!("{}", serde_json::to_string(event)?);
                Ok(())
            }
            Renderer::Progress(progress) => progress.handle_event(event),
        }
    }

    fn finish(&mut self) -> Result<()> {
        match self {
            Renderer::Progress(progress) => progress.finish(),
            _ => Ok(()),
        }
    }
}
```

This removes the current split between `render_jsonl_stdout(...)` and `render_progress_bars(...)`, or at least stops those functions from each owning the event loop.

---

## 2.4 Tests

Add pure resolver tests:

```rust
#[test]
fn ui_auto_uses_progress_when_tty() {
    assert_eq!(
        resolve_render_mode(UiMode::Auto, true),
        RenderMode::Progress
    );
}

#[test]
fn ui_auto_uses_quiet_when_not_tty() {
    assert_eq!(
        resolve_render_mode(UiMode::Auto, false),
        RenderMode::Quiet
    );
}
```

Add JSONL independence tests:

```rust
#[tokio::test]
async fn progress_jsonl_writes_file_in_quiet_mode();

#[tokio::test]
async fn progress_jsonl_writes_file_in_json_stdout_mode();

#[tokio::test]
async fn progress_jsonl_writes_file_in_progress_mode();
```

Expected behavior:

```text
--ui quiet --progress-jsonl file
  no progress bars
  no stdout JSON
  file contains events

--ui json --progress-jsonl file
  stdout JSON emitted
  file also contains events

--ui progress --progress-jsonl file
  progress bars shown
  file contains events
```

---

# 3. High Priority: Decide Whether to Accept Deferred Checkpointing

## Current Behavior

The code now avoids `blocking_send`, which is good.

But translation checkpointing currently happens after the scheduler returns:

```rust
let translations =
    translate_segments_with_callback(provider, segments, config, |_| Ok(())).await;

for translation in &translations {
    sender
        .send(make_checkpoint_command(&checkpoint, translation))
        .await?;
}
```

Batch mode similarly waits for `translate_batches_with_callback(...)` to return, then sends all checkpoint commands.

## Why This Is a Limitation

If the process crashes halfway through a long translation run:

```text
segments 1..200 translated in memory
process dies before scheduler returns
no checkpoint writes happened
progress lost
```

This is not the intended v1 behavior.

The original v1 target was:

```text
finalized SegmentTranslation produced
  -> checkpoint_tx.send(...).await
  -> continue
```

For batch mode:

```text
BatchWorkerResult
  -> coordinator finalizes SegmentTranslation
  -> checkpoint_tx.send(...).await
```

## Merge Policy Options

### Option A — Accept as intermediate PR

This PR can merge if:

- compile/clippy/tests pass;
- `--progress-jsonl` independence is fixed;
- this limitation is explicitly accepted.

In this case, add a note to the PR description:

```md
Known limitation:
Checkpoint writes currently happen after scheduler completion, not continuously as each segment finalizes. This avoids blocking async callbacks in this PR but means crash/interruption mid-scheduler may lose in-memory completed segment results. Follow-up PR will stream finalized SegmentTranslation values through checkpoint sender during execution.
```

### Option B — Fix before merge for true v1

Implement streaming checkpointing before merge.

For non-batch:

```rust
while let Some(result) = result_rx.recv().await {
    let translation = result?;
    checkpoint_sender
        .send(make_checkpoint_command(&checkpoint, &translation))
        .await?;
    translations.push(translation);
}
```

For batch:

```text
batch workers
  -> BatchWorkerResult
  -> coordinator
  -> split / retry / repair / aggregate
  -> finalized SegmentTranslation
  -> checkpoint_sender.send(...).await
  -> translations.push(...)
```

Do not checkpoint raw `BatchTranslationResult`.

## Recommendation

For speed, Option A is acceptable if this PR is treated as an intermediate merge, because the dangerous `blocking_send` issue has been removed.

For actual v1 readiness, Option B is required.

---

# 4. Medium Priority: Batch Scheduler Queue Fix Should Be Audited Once More

## Current Status

The previous all-work-before-results deadlock pattern has been improved. The batch code now tries to send work with `try_send`, then drains one result when `in_flight > 0`.

That is directionally correct.

## Remaining Concern

The code drains exactly one result per outer loop iteration after trying to fill the work queue. This is probably safe enough, but it should have a regression test proving bounded work/result queues cannot deadlock.

## Required Test

Add:

```rust
#[tokio::test]
async fn batch_scheduler_does_not_deadlock_when_work_and_result_queues_are_bounded()
```

Test shape:

```rust
#[tokio::test]
async fn batch_scheduler_does_not_deadlock_when_work_and_result_queues_are_bounded() {
    let run = async {
        // small concurrency, many batches, fast mock provider
        // enough batches to exceed work queue size and result queue pressure
        translate_batches_with_callback(...).await.unwrap();
    };

    tokio::time::timeout(std::time::Duration::from_secs(2), run)
        .await
        .expect("batch scheduler should not deadlock");
}
```

This test is required because the deadlock failure mode is subtle and will regress easily.

---

# 5. Medium Priority: `BatchSizer` Still Uses `eprintln!`

## Problem

`BatchSizer` emits operational updates through `eprintln!`.

This bypasses the progress dashboard and JSONL event stream.

## File

```text
crates/bookforge-llm/src/batch.rs
```

## Required Fix

For now, this can remain non-blocking, but it should eventually emit:

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

If you do not fix it in this PR, add it as a known follow-up. It is not a merge blocker.

---

# 6. Medium Priority: Request Progress `status` Is Too Coarse

## Problem

Batch scheduler emits:

```rust
status: if result.is_ok() { "ok" } else { "error" }
```

This loses diagnostics.

## File

```text
crates/bookforge-llm/src/batch.rs
```

## Better Status Mapping

Add helper:

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

Then:

```rust
status: result
    .as_ref()
    .map(|_| "ok")
    .unwrap_or_else(|err| request_status_from_error(err))
    .to_string()
```

This is not a merge blocker, but it directly improves the “why is it slow?” diagnostics.

---

# 7. Medium Priority: Default JSONL Path Is Still Not Job-Based

## Current Behavior

`--progress-jsonl` uses the supplied path. If absent, no JSONL file is written except possibly older `.bookforge/events.jsonl` behavior depending on code path.

The v1 plan target was:

```text
.bookforge/runs/<job_id>/events.jsonl
```

## Recommendation

Not required for merge if explicit `--progress-jsonl` works correctly in every UI mode.

For true v1, implement:

1. reporter starts without file path;
2. when it receives `ProgressEvent::JobCreated { job_id, ... }`, it opens:
   ```text
   .bookforge/runs/<job_id>/events.jsonl
   ```
3. if user provided `--progress-jsonl`, use that path instead.

This can be a follow-up PR.

---

# 8. Required Final Verification Commands

After fixes, run:

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

Also run:

```bash
rg "blocking_send" crates/bookforge-cli crates/bookforge-llm
rg "/dev/null" crates
rg "unbounded_channel" crates/bookforge-cli/src/checkpoint.rs
```

Expected:

```text
blocking_send:
  no production occurrences

/dev/null:
  no occurrences unless cfg-gated

unbounded_channel in checkpoint.rs:
  no occurrences
```

---

# 9. Minimum Merge Checklist

Merge only if all checked:

```text
[ ] progress.rs compiles; TranslationFinished dereferencing fixed.
[ ] --progress-jsonl writes in quiet mode.
[ ] --progress-jsonl writes in json stdout mode.
[ ] --progress-jsonl writes in progress mode.
[ ] --ui json prints events to stdout.
[ ] --ui auto remains TTY-aware.
[ ] JSONL flushes after important events and at shutdown.
[ ] cargo fmt passes.
[ ] cargo check passes.
[ ] cargo clippy passes.
[ ] cargo test passes.
[ ] Team accepts deferred-checkpointing limitation OR streaming checkpoints are implemented.
```

---

# 10. Suggested Implementation Order

```text
1. Fix TranslationFinished dereference compile issue.
2. Refactor progress reporter so JSONL file writing is independent of render mode.
3. Add tests for --progress-jsonl in quiet/json/progress modes.
4. Add batch bounded-queue non-deadlock test.
5. Run cargo fmt/check/clippy/test.
6. Decide deferred checkpointing policy:
   A. merge as intermediate with known limitation;
   B. implement streaming finalized checkpoints before merge.
7. Optional: improve batch request status mapping.
8. Optional: replace BatchSizer eprintln! with ProgressEvent::BatchSizingChanged.
```

---

# 11. Summary for Codex

The dangerous earlier issues are mostly fixed. The PR is now blocked by smaller but still real merge issues:

1. A likely compile error in `progress.rs`.
2. Incorrect `--progress-jsonl` behavior because file logging is tied to progress-bar mode.
3. No CI proof yet.
4. Checkpointing is safe but delayed until scheduler completion, which is acceptable only as an intermediate limitation.

Fix the compile issue and progress reporter architecture first. Then run the full Rust quality gate. Only after that decide whether to merge as an intermediate PR or implement streaming checkpoints for true v1.
