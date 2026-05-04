# toFinish.md — Remaining Work for BookForge

## 1. Wire adaptive concurrency into translate flow
- `AdaptiveLimiter` exists in `bookforge-llm/src/concurrency.rs` but is unused
- Need to integrate into `translate_batches_with_callback` and `translate_segments_with_callback`
- On 429: `limiter.on_rate_limit()` → reduce concurrency
- On timeout: `limiter.on_timeout()` → reduce concurrency
- On clean success: `limiter.on_success()` → increase toward max
- Replace static `Semaphore` with adaptive permitting

## 2. Fix batch translation bugs
- **Decode errors:** "error decoding response body" happens intermittently on larger batch requests — may need retry logic or connection pooling fixes
- **Repair items:** `collect_repair_items` collects failures but the repair prompt isn't invoked in the translate flow

## 3. Turbo-text-only mode implementation
- Profile exists with defaults but the mode itself (visible prose only, no markers) isn't implemented
- Need to strip markers from source before batching
- Need to skip marker validation in response parser

## 4. Caching for batch translations
- Cache matching includes `prompt_version` — batch prompts use `batch_v1` vs segment `v1`
- Batch translations won't reuse segment translations and vice versa
- Need: cache compatibility mapping or prompt-version-aware cache lookup

## 5. Integration test for batch translation
- No test covers the full batch translation flow
- Need: mock provider test that verifies batch build → request → parse → convert → checkpoint
- The `plain_blocks_batch_together` test only covers batching, not the full pipeline

## Done (Tier 1 + 2)

- [x] #10: Per-mode `max_output_tokens` multiplier (Plain=3x, MarkerSafe=4x, RunPreserving=5x, TurboTextOnly=2x)
- [x] #9: Ordinal preservation in batch output (carries segment.ordinal into SegmentTranslation)
- [x] #6: Timeout-aware batch sizing (FreeTier caps at 4096 output tokens)
- [x] #11: Clean up remaining warnings (no clippy warnings)
- [x] #3: Provider telemetry recording (TelemetryLog wired into batch flow, summary printed)
- [x] #4: Batch JSON failure → split retry (3-round retry queue with batch splitting)
- [x] #7: Clean provider config consolidation (single `provider_config()` helper)
