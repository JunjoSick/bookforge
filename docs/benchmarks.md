# Benchmarks

BookForge benchmarks should record both wall-clock time and the event-log counters that explain the run.

## Mock Smoke Benchmark

Use the deterministic mock provider to verify the release path without network access:

```bash
scripts/bench-mock.sh
```

The script writes:

```txt
/tmp/bookforge-bench-events.jsonl
/tmp/bookforge-bench.epub
```

By default the script uses `tests/fixtures/tiny.epub`, or `test/test.epub` when present in a local ignored workspace. Set `BOOKFORGE_BENCH_INPUT` to point at any tiny local EPUB fixture. Optional overrides:

```bash
BOOKFORGE_BENCH_EVENTS=/tmp/events.jsonl
BOOKFORGE_BENCH_OUTPUT=/tmp/output.epub
```

## Metrics To Capture

For provider benchmarks, capture:

- elapsed time
- request count
- p50 and p95 latency
- 429s, timeouts, invalid JSON, and truncations
- input/output tokens
- tokens per minute and blocks per minute
- batch split count and repair count

Real-provider scripts must require API keys through environment variables and must not print key values.
