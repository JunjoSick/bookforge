# bookforge_fixes.md — Codex Implementation Brief

Repository: `JunjoSick/bookforge`  
Target branch: `main`  
Purpose: fix real correctness, concurrency, cache, and checkpointing bugs in BookForge without turning the codebase into a larger rewrite than necessary.

This file incorporates the reviewer suggestions plus the extra edge-case notes:

- remove the redundant `Err(LlmError::InvalidResponse(msg)) if msg.contains("truncated") => Err(...)` arm;
- make checkpoint-writer error semantics explicit;
- decide how strict cache invalidation should be;
- ensure `AdaptivePermit::Drop` cannot panic or underflow;
- preserve original block IDs in batch output.

---

## Implementation order

Implement in this order:

1. Batch truncation and batch block-ID correctness.
2. Provider retry configuration propagation.
3. Adaptive limiter starvation fix.
4. Cache namespace / dirty-bit invalidation.
5. XML validation via `quick_xml`.
6. Async-safe checkpoint writer actor for SQLite.

Reasoning:

- Tasks 1–3 are immediate correctness/concurrency fixes.
- Cache invalidation should land before more segmentation or inline-marker changes.
- XML validation is isolated and low-risk.
- The checkpoint writer actor is architecturally important but touches more code, so do it after the smaller correctness fixes.

---

# Task 1 — Fix batch truncation handling

## File

`crates/bookforge-llm/src/batch.rs`

## Problem

The non-batch scheduler checks `FinishReason::Length` and treats it as a truncation error:

```rust
if response.finish_reason == FinishReason::Length {
    return Err(LlmError::InvalidResponse(
        "output was truncated: max_output_tokens limit reached".to_string(),
    ));
}
```

The batch path does not. It calls `provider.complete`, then immediately parses `resp.content`.

This is wrong because OpenAI-compatible providers normally report truncation as a successful response with:

```text
finish_reason = "length"
```

not as a provider error.

A truncated batch can therefore be misclassified as:

- missing batch items;
- invalid JSON;
- item-level repair failures;
- or a successful `BatchTranslationResult` with failures.

That prevents the existing split fallback from doing its job.

Current split fallback:

```rust
Ok((batch, Err(LlmError::InvalidResponse(_)))) if batch.items.len() > 1 => {
    pending.extend(split_batch(&batch));
}
```

So the correct behavior is:

- batch-level truncation must become `Err(LlmError::InvalidResponse(...))`;
- the scheduler then splits multi-item batches;
- only after splitting is exhausted should items go to failure/repair paths.

## Required patch

Import `FinishReason` if needed:

```rust
use crate::{
    AdaptiveLimiter, CompletionRequest, FinishReason, LlmError, LlmProvider, PromptLibrary,
    RequestMetadata, ResponseFormat, SegmentTranslation, Substitutions, TelemetryLog,
    TranslationRunConfig,
};
```

Update `translate_one_batch` to this shape:

```rust
match response {
    Ok(resp) => {
        if resp.finish_reason == FinishReason::Length {
            return Err(LlmError::InvalidResponse(
                "batch output was truncated: max_output_tokens limit reached".to_string(),
            ));
        }

        let mut result = parse_batch_response(&batch, &resp.content)
            .map_err(LlmError::InvalidResponse)?;
        result.input_tokens = resp.input_tokens;
        result.output_tokens = resp.output_tokens;
        Ok(result)
    }

    Err(e) => Err(e),
}
```

Do **not** keep this redundant arm:

```rust
Err(LlmError::InvalidResponse(msg)) if msg.contains("truncated") => {
    Err(LlmError::InvalidResponse(msg))
}
```

It returns the exact same error and is fully covered by:

```rust
Err(e) => Err(e)
```

Less code means less chance of accidentally reintroducing the swallow bug.

## Important: remove the truncation swallow block

Delete the current code that converts truncation into a successful batch result:

```rust
Err(LlmError::InvalidResponse(msg)) if msg.contains("truncated") => {
    let result = BatchTranslationResult {
        batch_id: batch.id.clone(),
        translations: Vec::new(),
        failures: batch.items.iter().map(...).collect(),
        input_tokens: None,
        output_tokens: None,
    };
    Ok(result)
}
```

This is the bug.

## Tests

Add:

```rust
#[tokio::test]
async fn batch_length_finish_reason_triggers_split_not_repair()
```

Expected:

- mock provider returns `CompletionResponse { finish_reason: FinishReason::Length, ... }`;
- multi-item batch produces an `InvalidResponse`;
- scheduler split path is exercised;
- it does not immediately convert all items into `BatchItemFailure`.

Add:

```rust
#[tokio::test]
async fn batch_truncated_error_is_not_swallowed()
```

Expected:

- provider returns `Err(LlmError::InvalidResponse("truncated".into()))`;
- `translate_one_batch` returns the same error;
- it does not create a successful `BatchTranslationResult`.

---

# Task 1b — Preserve original block IDs in batch output

## File

`crates/bookforge-llm/src/batch.rs`

## Problem

Batch item IDs are compound IDs:

```rust
item_id: format!("{}:{}", segment.id.0, block.block_id.0)
```

But successful batch output is currently converted like this:

```rust
entry.blocks.push(BlockTranslation {
    block_id: BlockId(translation.item_id.clone()),
    text: translation.text.clone(),
});
```

That means block IDs become:

```text
seg_...:b_000123
```

instead of:

```text
b_000123
```

The EPUB rebuild code expects original IR block IDs. If batch-mode translations use compound item IDs as block IDs, rebuild can fail to apply translations.

## Required patch

Use `all_items` to recover the original `block_id`.

Replace:

```rust
entry.blocks.push(BlockTranslation {
    block_id: BlockId(translation.item_id.clone()),
    text: translation.text.clone(),
});
```

with:

```rust
if let Some(source_item) = all_items.get(&translation.item_id) {
    entry.blocks.push(BlockTranslation {
        block_id: source_item.block_id.clone(),
        text: translation.text.clone(),
    });
}
```

If the item is missing from `all_items`, treat it as an invalid internal state. Prefer recording a failure or logging loudly rather than silently creating a bogus block ID.

## Test

Add:

```rust
#[tokio::test]
async fn batch_translation_preserves_original_block_ids()
```

Expected:

- produced `SegmentTranslation.blocks[*].block_id` equals the original `BlockId`;
- no block ID contains the segment prefix;
- block IDs are usable by EPUB rebuild.

---

# Task 2 — Thread `provider_max_attempts` into the provider

## Files

- `crates/bookforge-llm/src/provider.rs`
- `crates/bookforge-cli/src/commands/translate.rs`
- `crates/bookforge-core/src/config.rs`

## Problem

`OpenAiCompatibleProvider::complete` currently uses:

```rust
let max_attempts = 6usize;
```

This ignores:

- profile defaults;
- `ProviderRuntimeConfig.provider_max_attempts`;
- CLI `--provider-max-attempts`.

## Required patch

Add `provider_max_attempts` to `OpenAiCompatibleConfig`:

```rust
#[derive(Debug, Clone)]
pub struct OpenAiCompatibleConfig {
    pub base_url: String,
    pub api_key_env: String,
    pub model: String,
    pub timeout_seconds: u64,
    pub provider_max_attempts: usize,
}
```

Set a default in constructors:

```rust
provider_max_attempts: 6,
```

In `complete`:

```rust
let max_attempts = self.config.provider_max_attempts.max(1);
```

Update `provider_config(...)` in the CLI to accept and pass the configured value:

```rust
fn provider_config(
    provider: &str,
    model: Option<&str>,
    base_url: Option<&str>,
    api_key_env: Option<&str>,
    timeout_seconds: u64,
    provider_max_attempts: usize,
) -> Result<OpenAiCompatibleConfig> {
    ...
    Ok(OpenAiCompatibleConfig {
        base_url,
        api_key_env,
        model,
        timeout_seconds,
        provider_max_attempts: provider_max_attempts.max(1),
    })
}
```

Update all call sites:

- primary provider;
- fallback provider;
- QA provider;
- double-check provider.

## Timeout cap

The existing logic caps timeout retries:

```rust
fn attempt_limit_for_http_error(error: &reqwest::Error, max_attempts: usize) -> usize {
    if error.is_timeout() { 2 } else { max_attempts }
}
```

If this behavior is kept, make it explicit and underflow-safe:

```rust
fn attempt_limit_for_http_error(error: &reqwest::Error, max_attempts: usize) -> usize {
    if error.is_timeout() {
        max_attempts.min(2)
    } else {
        max_attempts
    }
}
```

## Tests

Add:

```rust
#[test]
fn provider_config_sets_provider_max_attempts()
```

If HTTP test infrastructure exists, add:

```rust
#[tokio::test]
async fn provider_respects_configured_max_attempts_for_retryable_status()
```

Expected:

- configure `provider_max_attempts = 2`;
- fake endpoint returns `429`;
- exactly two HTTP calls occur.

---

# Task 3 — Fix `AdaptiveLimiter` starvation

## Files

- `crates/bookforge-llm/src/concurrency.rs`
- `crates/bookforge-llm/src/batch.rs`

## Problem

Current shrink logic:

```rust
tokio::spawn(async move {
    if let Ok(permit) = sem.acquire_many_owned(delta).await {
        permit.forget();
    }
});
```

This can starve workers because Tokio semaphores are FIFO. A large `acquire_many_owned(delta)` can sit at the queue head waiting for many permits, blocking later one-permit worker acquires.

## Required design

Do not shrink by acquiring many permits.

Shrink lazily with a burn counter:

- on shrink, increment `permits_to_burn`;
- when a worker finishes, its RAII permit wrapper checks the counter;
- if burn is pending, it calls `permit.forget()` instead of releasing the permit.

## Required implementation

In `concurrency.rs`:

```rust
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
```

Add:

```rust
pub struct AdaptiveLimiter {
    state: Mutex<usize>,
    min: usize,
    max: usize,
    semaphore: Arc<Semaphore>,
    permits_to_burn: Arc<AtomicUsize>,
}
```

Add an RAII permit:

```rust
pub struct AdaptivePermit {
    permit: Option<OwnedSemaphorePermit>,
    permits_to_burn: Arc<AtomicUsize>,
}
```

Panic-safe `Drop` implementation:

```rust
impl Drop for AdaptivePermit {
    fn drop(&mut self) {
        let should_burn = self
            .permits_to_burn
            .fetch_update(
                Ordering::AcqRel,
                Ordering::Acquire,
                |n| if n > 0 { Some(n - 1) } else { None },
            )
            .is_ok();

        if should_burn {
            if let Some(permit) = self.permit.take() {
                permit.forget();
            }
        }
    }
}
```

Important: do **not** implement this with unchecked `fetch_sub(1)`. That can underflow if the counter is already zero. A panic in `Drop` can double-panic the runtime during unwinding. The `fetch_update` sketch above is safe.

Add:

```rust
impl AdaptiveLimiter {
    pub async fn acquire(&self) -> Result<AdaptivePermit, tokio::sync::AcquireError> {
        let permit = self.semaphore.clone().acquire_owned().await?;
        Ok(AdaptivePermit {
            permit: Some(permit),
            permits_to_burn: self.permits_to_burn.clone(),
        })
    }
}
```

Shrink:

```rust
} else if new < *state {
    let delta = *state - new;
    *state = new;
    self.permits_to_burn.fetch_add(delta, Ordering::AcqRel);
}
```

Grow should preferably cancel pending burns before adding permits. If implementing the simpler version, document that rapid shrink/grow can temporarily underutilize concurrency.

Preferred grow behavior:

```rust
if new > *state {
    let mut remaining_to_add = new - *state;
    *state = new;

    while remaining_to_add > 0 {
        match self.permits_to_burn.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |burn| {
                if burn == 0 {
                    None
                } else {
                    let cancel = burn.min(remaining_to_add);
                    Some(burn - cancel)
                }
            },
        ) {
            Ok(previous_burn) => {
                let cancelled = previous_burn.min(remaining_to_add);
                remaining_to_add -= cancelled;
            }
            Err(_) => break,
        }
    }

    if remaining_to_add > 0 {
        self.semaphore.add_permits(remaining_to_add);
    }
}
```

## Update batch scheduler

The adaptive path must not keep acquiring a raw semaphore permit, because raw permits do not know about the burn counter.

Replace adaptive raw semaphore usage with `AdaptiveLimiter::acquire()`.

Use a wrapper enum if needed:

```rust
enum BatchPermit {
    Adaptive(AdaptivePermit),
    Fixed(tokio::sync::OwnedSemaphorePermit),
}
```

For fixed concurrency, raw semaphore acquire is fine.

For adaptive concurrency:

```rust
let _permit = limiter.acquire().await.map_err(|_| {
    LlmError::Provider("scheduler semaphore closed".to_string())
})?;
```

## Tests

Add:

```rust
#[tokio::test]
async fn adaptive_limiter_shrink_does_not_enqueue_large_acquire()

#[tokio::test]
async fn adaptive_limiter_burns_released_permits_after_shrink()

#[tokio::test]
async fn adaptive_permit_drop_does_not_underflow_or_panic()
```

Use `tokio::time::timeout` to prove a single acquire is not blocked behind a hidden large acquire.

---

# Task 4 — Cache namespace / dirty-bit invalidation

## Files

- `crates/bookforge-core/src/segment.rs`
- `crates/bookforge-core/src/config.rs`
- `crates/bookforge-store/src/db.rs`
- `crates/bookforge-cli/src/commands/translate.rs`

## Problem

Cache lookup currently uses source hash, prompt version, provider, model, and language. The segment checksum is based on source text only.

That misses internal compatibility factors:

- segment schema;
- block schema;
- inline marker extraction version;
- segmentation config;
- context config;
- batch/profile behavior;
- prompt-mode compatibility.

An old cache row can be reused even when the IR shape is no longer compatible.

## Strict vs lenient invalidation

There are two possible strategies.

### Option A — strict invalidation

Hash both schema versions and segmentation parameters:

```text
cache_namespace = hash(
  CACHE_KEY_SCHEMA_VERSION,
  SEGMENT_SCHEMA_VERSION,
  INLINE_MARKER_SCHEMA_VERSION,
  max_segment_tokens,
  context_tokens,
  profile,
  batch_enabled,
  prompt_version
)
```

Pros:

- safest;
- simple;
- no accidental reuse across segmentation regimes.

Cons:

- changing `--max-segment-tokens 1200` to `1201` invalidates the whole book cache, even if many block groups are unchanged.

### Option B — lenient invalidation

Hash only schema/extraction versions and prompt/cache versions:

```text
cache_namespace = hash(
  CACHE_KEY_SCHEMA_VERSION,
  SEGMENT_SCHEMA_VERSION,
  INLINE_MARKER_SCHEMA_VERSION,
  prompt_version
)
```

Then rely on exact block-ID compatibility validation to reject bad hits.

Pros:

- better resume behavior when token settings change slightly;
- can reuse translations for structurally identical segments.

Cons:

- slightly more subtle;
- requires very strict block-ID validation;
- context-sensitive translations may be reused even when context window changes.

## Recommendation

Default to Option A for now. It is stricter but safer.

If the product needs better resume behavior across tiny segmentation setting changes, implement Option B only with exact block-ID matching and a clear comment explaining the tradeoff.

Given this tool is still evolving quickly, correctness beats cache reuse.

## Required DB migration

Add to `segments`:

```sql
cache_namespace TEXT NOT NULL DEFAULT ''
```

Migration:

```rust
ensure_column(&conn, "segments", "cache_namespace", "TEXT NOT NULL DEFAULT ''")?;
```

Add index:

```sql
CREATE INDEX IF NOT EXISTS idx_segments_cache_lookup
ON segments(source_hash, cache_namespace, prompt_version, provider, model, status);
```

Update `insert_segments`:

```rust
pub fn insert_segments(
    &self,
    job_id: &str,
    segments: &[Segment],
    prompt_version: &str,
    provider: &str,
    model: &str,
    cache_namespace: &str,
) -> Result<()>
```

Store the namespace.

Update `find_cached_translation`:

```rust
pub fn find_cached_translation(
    &self,
    segment: &Segment,
    prompt_version: &str,
    provider: &str,
    model: &str,
    source_lang: Option<&str>,
    target_lang: &str,
    cache_namespace: &str,
) -> Result<Option<CachedTranslation>>
```

SQL must include:

```sql
AND s.cache_namespace = ?N
```

## Exact block-ID compatibility

Before accepting a cached hit, validate block IDs:

```rust
let expected = segment
    .block_ids
    .iter()
    .map(|id| id.0.as_str())
    .collect::<Vec<_>>();

let actual = blocks
    .iter()
    .map(|block| block.block_id.0.as_str())
    .collect::<Vec<_>>();

if expected != actual {
    return Ok(None);
}
```

This is required even with strict invalidation.

## Cross prompt-version fallback

Current code attempts fallback between `"batch_v1"` and `"v1"`.

For correctness, remove this fallback unless both of these are true:

1. `cache_namespace` matches;
2. exact block-ID compatibility passes.

Preferred for this PR: remove the fallback. Reintroduce later only if needed.

## Tests

Add:

```rust
#[test]
fn cache_namespace_changes_when_segmentation_settings_change()

#[test]
fn cached_translation_requires_matching_cache_namespace()

#[test]
fn cached_translation_rejects_mismatched_block_ids()

#[test]
fn old_empty_cache_namespace_rows_do_not_match_new_runs()
```

If implementing lenient mode instead, rename the first test accordingly:

```rust
#[test]
fn cache_namespace_does_not_change_for_token_settings_but_block_ids_are_validated()
```

---

# Task 5 — Replace manual XML validation with `quick_xml`

## File

`crates/bookforge-epub/src/validate.rs`

## Problem

`has_broken_xml` manually parses XML by scanning strings and maintaining a tag stack. This is brittle and redundant. `writer.rs` already uses `quick_xml::Reader` for parser dry-runs.

## Required patch

Replace `has_broken_xml` with:

```rust
use quick_xml::{events::Event, Reader};

fn has_broken_xml(content: &str) -> bool {
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(false);

    loop {
        match reader.read_event() {
            Ok(Event::Eof) => return false,
            Ok(_) => continue,
            Err(_) => return true,
        }
    }
}
```

Optional but better: return the error string so reports are more useful.

```rust
fn xml_validation_error(content: &str) -> Option<String> {
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(false);

    loop {
        match reader.read_event() {
            Ok(Event::Eof) => return None,
            Ok(_) => continue,
            Err(err) => return Some(err.to_string()),
        }
    }
}
```

## Tests

Add:

```rust
#[test]
fn accepts_self_closing_tags_with_attributes()

#[test]
fn accepts_namespaced_xhtml()

#[test]
fn accepts_comments_and_processing_instructions()

#[test]
fn rejects_broken_xml()
```

---

# Task 6 — Async-safe checkpoint writer actor for SQLite

## Files

- `crates/bookforge-store/src/db.rs`
- `crates/bookforge-cli/src/commands/translate.rs`
- optionally new file: `crates/bookforge-cli/src/checkpoint.rs` or `crates/bookforge-store/src/checkpoint.rs`

## Problem

`JobStore` uses synchronous `rusqlite` through a `RefCell<Connection>`. Checkpoint callbacks call store writes while async translation orchestration is running.

This blocks Tokio worker/orchestrator progress on disk I/O.

## Recommended architecture

Do not migrate to `sqlx` or `deadpool-sqlite` yet. Use a single SQLite writer actor.

This keeps SQLite single-writer semantics and avoids a huge rewrite.

## Design

Create a checkpoint command enum:

```rust
pub enum CheckpointCommand {
    SaveTranslation {
        job_id: String,
        translation: SegmentTranslation,
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
```

Create a writer handle:

```rust
pub struct CheckpointWriter {
    tx: tokio::sync::mpsc::UnboundedSender<CheckpointCommand>,
    join: tokio::task::JoinHandle<anyhow::Result<()>>,
}
```

Spawn with `spawn_blocking`:

```rust
let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<CheckpointCommand>();
let db_path = db_path.clone();

let join = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
    let store = JobStore::open(db_path)?;

    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            CheckpointCommand::SaveTranslation { ... } => {
                save_translation_result_sync(&store, ...)?;
            }
            CheckpointCommand::MarkFailed { ... } => {
                store.mark_segment_failed(...)?;
            }
        }
    }

    Ok(())
});
```

The scheduler callback should only enqueue:

```rust
move |translation| {
    tx.send(CheckpointCommand::SaveTranslation { ... })
        .map_err(|err| LlmError::Provider(format!("checkpoint queue closed: {err}")))
}
```

After translation ends:

```rust
drop(tx);
join.await??;
```

This flushes all queued checkpoints before rebuild/reporting.

## Error semantics: important tradeoff

This pattern is intentionally fire-and-forget per segment. That avoids blocking the scheduler, but changes failure shape.

If SQLite fails on segment 5, for example due to disk full:

1. the blocking writer returns `Err`;
2. the writer task exits;
3. the channel closes;
4. translation workers for segments 6–100 may continue briefly;
5. their next checkpoint send fails with a generic queue-closed error;
6. final `join.await??` must surface the original SQLite error if available.

This is acceptable, but implement it deliberately.

## Required error handling

Do not silently drop the original writer error.

The final await must report the writer task error:

```rust
let writer_result = writer.join.await
    .map_err(|err| anyhow::anyhow!("checkpoint writer task join failed: {err}"))?;

writer_result
    .map_err(|err| anyhow::anyhow!("checkpoint writer failed: {err}"))?;
```

Also, when `send` fails:

```rust
LlmError::Provider("checkpoint queue closed; checkpoint writer may have failed".to_string())
```

Do not pretend the individual segment itself failed due to provider behavior. The log should make it clear that checkpoint persistence failed.

## Optional improvement

Use a shared cancellation flag:

```rust
Arc<AtomicBool> checkpoint_failed
```

When the writer fails, set the flag. The callback checks it and returns an explicit error earlier. This avoids translating too many extra segments after DB death.

This is optional, not required for the first implementation.

## Tests

Add:

```rust
#[tokio::test]
async fn checkpoint_writer_flushes_all_translations_before_shutdown()

#[tokio::test]
async fn checkpoint_writer_surfaces_original_sqlite_error_on_join()

#[tokio::test]
async fn checkpoint_send_fails_when_writer_exits()
```

Acceptance criteria:

- no direct SQLite writes in the hot async callback;
- checkpoints flush before rebuild/report;
- queue-closed errors are understandable;
- original writer error is surfaced at final join.

---

# Final test suite

Run:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

If the workspace does not support `--all-features`:

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Manual smoke test:

```bash
cargo run -- translate path/to/sample.epub   --target Italian   --provider mock   --model mock-prefix-target   --profile balanced
```

Provider retry smoke test with a real or stubbed endpoint:

```bash
cargo run -- translate path/to/sample.epub   --target Italian   --provider openrouter   --model <model>   --provider-max-attempts 1   --concurrency 2
```

Expected: one provider-level HTTP attempt per request.

---

# Acceptance checklist

- [ ] Batch `FinishReason::Length` returns `Err(LlmError::InvalidResponse(...))`.
- [ ] Redundant truncation match arm is not present.
- [ ] Truncation is not converted into item-level repair failures before splitting.
- [ ] Batch translations preserve original block IDs.
- [ ] Provider retry count comes from `OpenAiCompatibleConfig.provider_max_attempts`.
- [ ] CLI `--provider-max-attempts` reaches primary, fallback, QA, and double-check providers.
- [ ] Adaptive limiter shrink uses burn-on-release, not spawned `acquire_many_owned`.
- [ ] `AdaptivePermit::Drop` is panic-safe and underflow-safe.
- [ ] Adaptive scheduler path uses `AdaptiveLimiter::acquire()`, not raw semaphore acquisition.
- [ ] Cache lookup includes a namespace/dirty bit.
- [ ] Cached block IDs must exactly match current segment block IDs.
- [ ] Cross prompt-version cache fallback is removed or namespace+layout guarded.
- [ ] XML validation uses `quick_xml::Reader`.
- [ ] SQLite checkpointing uses a writer actor or equivalent async-safe boundary.
- [ ] Checkpoint writer surfaces original DB errors on final join.
- [ ] `cargo fmt`, `cargo clippy`, and `cargo test` pass.
