# toFinish.md — Remaining Work for BookForge

## 1. Wire adaptive concurrency into translate flow
- `AdaptiveLimiter` exists in `bookforge-llm/src/concurrency.rs` but is unused
- Need to integrate into `translate_batches_with_callback` and `translate_segments_with_callback`
- On 429: `limiter.on_rate_limit()` → reduce concurrency
- On timeout: `limiter.on_timeout()` → reduce concurrency
- On clean success: `limiter.on_success()` → increase toward max
- Replace static `Semaphore` with adaptive permitting

## 2. Fix batch translation bugs
- **Truncated JSON:** Increase `max_output_tokens` multiplier (currently 5x, may need 8x for some models)
- **Decode errors:** "error decoding response body" happens intermittently on larger batch requests — may need retry logic or connection pooling fixes
- **Batch splitting on failure:** When a batch fails, `split_batch` exists but isn't called automatically in the retry loop
- **Repair items:** `collect_repair_items` collects failures but the repair prompt isn't invoked in the translate flow

## 3. Provider telemetry recording
- `TelemetryLog` exists in `bookforge-llm/src/telemetry.rs` but isn't populated
- Need to call `log.record()` in `translate_one_batch` and `request_translation` with `ProviderRequestMetric`
- Print summary in report output (p50/p95 latency, 429 count, timeout count, total backoff)

## 4. Batch JSON failure resilience
- When batch response is invalid JSON:
  1. Retry same batch once (done in provider retry loop)
  2. If still invalid, `split_batch` and retry halves (not wired)
  3. Single-item failure → mark `needs_review` (not wired)
- Currently one bad batch poisons the whole run via error propagation in `translate_batches_with_callback`

## 5. Turbo-text-only mode implementation
- Profile exists with defaults but the mode itself (visible prose only, no markers) isn't implemented
- Need to strip markers from source before batching
- Need to skip marker validation in response parser

## 6. Timeout-aware batch sizing
- Batch token estimates affect `max_output_tokens` but don't consider the timeout
- Large batches may time out on slow/free-tier providers
- Need: if `profile == FreeTier`, cap `max_output_tokens` lower (e.g. 4096) and batch smaller

## 7. Provider-specific configuration
- Some providers need different settings (e.g., DeepSeek needs `DEEPSEEK_API_KEY`, OpenRouter needs `OPENROUTER_API_KEY`)
- The CLI could auto-detect or provide a `--provider-config` flag
- Currently requires manual `--api-key-env` override

## 8. Caching for batch translations
- Cache matching includes `prompt_version` — batch prompts use `batch_v1` vs segment `v1`
- Batch translations won't reuse segment translations and vice versa
- Need: cache compatibility mapping or prompt-version-aware cache lookup

## 9. Segment ordinal preservation in batch output
- `translate_batches_with_callback` aggregates by `segment_id` but loses original `ordinal`
- Segment translations come back unsorted; caller sorts by `ordinal` but batch results have `ordinal: 0`
- Need: carry ordinal from source segments into batch result aggregation

## 10. Batch `max_output_tokens` per-mode refinement
- Different batch modes need different multipliers:
  - Plain: 3x source tokens
  - MarkerSafe: 4x (markers add overhead)
  - RunPreserving: 5x (structured response)
  - TurboTextOnly: 2x (no markers)
- Currently hardcoded `5x` for all modes

## 11. Clean up remaining warnings
- `bookforge-llm/src/batch.rs`: unused variable warnings fixed but need verification
- `bookforge-epub/src/validate.rs`: minor clippy suggestions applied but may need revisit

## 12. Integration test for batch translation
- No test covers the full batch translation flow
- Need: mock provider test that verifies batch build → request → parse → convert → checkpoint
- The `plain_blocks_batch_together` test only covers batching, not the full pipeline
