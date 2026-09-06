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

## Dense inline-marker reconstruction (2026-09-06)

Run the dependency-free, opt-in writer microbenchmark:

```bash
cargo test --release -p bookforge-epub --lib benchmark_dense_inline_markers --locked -- --ignored --nocapture
```

It reconstructs adjacent translated spans and restores their source whitespace.
Fixture construction is outside the timer; reconstruction, output allocation,
and output destruction are inside. Each sample runs 20 reconstructions. The
benchmark asserts event counts, never a machine-speed threshold. Normal tests
separately check exact reconstructed XHTML, duplicate markers, empty spans,
Unicode, punctuation, and malformed/nested marker handling.

Measured locally on Linux x86-64, Rust 1.98.0, optimized release builds. Results
are medians of five trials per implementation, alternating before/after order.
The before snapshot is the approved cleanup before these writer optimizations;
the after snapshot uses bounded closing-marker search, set membership for used
markers, and sequential whitespace restoration. Baseline and candidate builds
must use separate `CARGO_TARGET_DIR` paths: sharing a target directory between
source copies can reuse stale artifacts when source timestamps differ.

| Markers in a passage | Before, 20 reconstructions | After, 20 reconstructions | Speedup |
| --- | ---: | ---: | ---: |
| 32 | 1.455 ms | 1.264 ms | 1.15× |
| 256 | 22.352 ms | 10.419 ms | 2.15× |
| 2,048 | 932.638 ms | 91.892 ms | 10.15× |

The largest gain comes from avoiding scans over all later sibling markers while
locating each closing marker. Used-marker checks also avoid repeated linear
searches; whitespace restoration avoids shifting the event vector on every
insertion and borrows decoded text where possible. It uses linear scratch
storage for following-character lookup. Output ordering and sorted missing-marker
diagnostics remain deterministic.

These are local writer microbenchmarks, not whole-book or provider-throughput
claims. Typical prose with few markers improves less; live translation time may
still be dominated by the provider. Use supplied-book runs to assess practical
end-to-end impact.
