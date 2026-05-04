# toFinish.md — Remaining Work for BookForge

## 1. Investigate batch decode errors
- "error decoding response body" happens intermittently on larger batch requests
- May be connection pooling, compression, or request size issues
- Need diagnostic logging in provider's `complete()` to isolate root cause

## 2. Caching for batch translations
- Cache matching uses `prompt_version` — batch uses `batch_v1`, segment uses `v1`
- Batch translations won't reuse segment translations and vice versa
- Fix: add prompt-version-aware cache lookup in `find_cached_translation`

## 3. Integration test for batch translation
- No test covers the full batch translation flow
- Need: mock provider test that verifies batch build → request → parse → convert → checkpoint

## Done

- [x] Tier 1: Per-mode token multiplier, ordinal preservation, FreeTier timeout capping
- [x] Tier 2: Provider config consolidation, batch split retry, telemetry recording
- [x] Tier 3: Adaptive concurrency wiring, turbo-text-only mode
- [x] Tier 4: Batch repair with dedicated repair prompt
