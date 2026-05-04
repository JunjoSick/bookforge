# toFinish.md — Remaining Work for BookForge

## 1. Batch repair with dedicated repair prompt
- `collect_repair_items` collects failures but the repair prompt (`translate_batch_repair.v1.md`) isn't invoked
- Need: after batch translation, group failure items into repair batches and send to provider with repair prompt
- Validate repairs with same rules; apply valid ones, mark unresolved ones `needs_review`

## 2. Investigate batch decode errors
- "error decoding response body" happens intermittently on larger batch requests
- May be connection pooling, compression, or request size issues
- Need diagnostic logging to isolate root cause

## 3. Caching for batch translations
- Cache matching uses `prompt_version` — batch uses `batch_v1`, segment uses `v1`
- Batch translations won't reuse segment translations and vice versa
- Need: prompt-version-aware cache lookup or compatibility mapping

## 4. Integration test for batch translation
- No test covers the full batch translation flow
- Need: mock provider test that verifies batch build → request → parse → convert → checkpoint
- The `plain_blocks_batch_together` test only covers batching, not the full pipeline

## Done

- [x] Tier 1: Per-mode token multiplier, ordinal preservation, FreeTier timeout capping
- [x] Tier 2: Provider config consolidation, batch split retry, telemetry recording
- [x] Tier 3: Adaptive concurrency wiring, turbo-text-only mode (marker stripping, skip validation)
