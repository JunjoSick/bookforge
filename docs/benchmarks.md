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

## Glossary selection (2026-09-07)

```bash
cargo test --release -p bookforge-core --lib benchmark_glossary_selection --locked -- --ignored --nocapture
```

The fixture uses 128 case-insensitive person terms, two matching names per
segment, roughly 750 bytes of prose per segment, a section boundary every 25
segments, and an 800-token glossary budget. Each trial includes three complete
selections (scope merging, counts, rule selection, budgeting, and output
allocation/destruction); fixture construction is outside the timer.

Local Linux x86-64 / Rust 1.98.0 release-build medians from five trials per
implementation, alternating before/after order:

| Segments | Before, three selections | After, three selections | Speedup |
| --- | ---: | ---: | ---: |
| 100 | 45.907 ms | 12.651 ms | 3.63× |
| 1,000 | 468.476 ms | 113.914 ms | 4.11× |

The baseline is `313e4b5` (glossary source unchanged from `e020e2e`). The candidate
prepares lowercase term strings once, lowercases each segment once per pass,
and retains only the previous five segments' ordered matching term indices.
It scans each segment twice for global counts and direct selection instead of
rescanning previous text for recent-term selection. Scratch storage for recent
matches is bounded by five segments, rather than the whole book.

A development differential probe compared complete selection structures with
the original implementation in 960 cases: 160 deterministic fixtures across six
token budgets. These cover Unicode casing, empty sources, mixed case policies,
repeated IDs, scope precedence, section transitions, always-active terms, and
frequency anchors. All matched. Permanent regressions check Unicode matching
and the ordered five-position recent window alongside existing scope/rule/budget
tests. The benchmark has no machine-speed assertion.

Glossary selection is used by prompt preparation and cache identity context
loading. These results measure that shared operation, not database I/O, provider
latency, or end-to-end book translation. Baseline and candidate test executables
were saved separately before alternating runs.

## Batch planning and runtime queue rebuilding (2026-09-07)

```bash
cargo test --release -p bookforge-llm --lib benchmark_batch_planning --locked -- --ignored --nocapture
```

The fixture uses one chapter with 1,000 or 4,000 items, each containing 744 bytes
of prose. Initial packing uses 16,000 tokens / 64 items; runtime rebuilding uses
8,000 tokens / 32 items. Both account for an 8,000-token output cap. Each trial
runs the operation three times, with input construction/cloning and output
assertions/destruction outside the timer. Medians of five alternating trials
per implementation on Linux x86-64, Rust 1.98.0, optimized release builds:

| Operation | Items | Before, three runs | After, three runs | Speedup |
| --- | ---: | ---: | ---: | ---: |
| Runtime queue rebuild | 1,000 | 298.763 ms | 7.735 ms | 38.63× |
| Runtime queue rebuild | 4,000 | 1,865.306 ms | 30.382 ms | 61.40× |
| Prompt-aware repacking | 1,000 | 216.560 ms | 7.331 ms | 29.54× |
| Prompt-aware repacking | 4,000 | 871.200 ms | 30.327 ms | 28.73× |

Baseline: `8c68679`. Runtime regrouping no longer estimates every growing prefix
of a chapter before discarding those estimates during repacking. Repacking
accumulates source, retry-guidance, and output-envelope costs instead of scanning
all earlier item text again. Glossary overhead remains calculated from each
candidate batch to preserve cross-item deduplication. Initial section/mode sorting
also caches each group's earliest ordinal; its gain is not isolated in this table.

A development comparison checked complete repacked and repartitioned batch
structures against the original implementation in 400 cases (four modes, 20
fixtures, five token limits). IDs, ordinals, repair/translation kinds, item order,
section IDs, boundaries, and token estimates matched. Permanent tests retain
mixed Unicode/empty/oversized items, guidance, overlapping JSON/prose glossaries,
item limits, and output caps, alongside existing live reconfiguration tests.

These measurements concern local planning, not provider throughput. No speed
threshold is imposed on correctness tests. Before/after executables were saved
separately, and final timing trials ran without concurrent compilation.

## Job summary scans (2026-09-07)

```bash
cargo test --release -p bookforge-store --lib benchmark_job_summaries --locked -- --ignored --nocapture
```

This uses the actual SQLite-backed `list_job_summaries` API on 8 and 64 jobs,
with 1,000 segments per job. Half have an attempt-ledger row; the rest use legacy
token columns. Status and retry counts vary. Database creation and population
are outside the timer; three listings, their allocation/destruction, and result
assertions are timed. Five alternating release trials on the same Linux/Rust
setup above gave these medians:

| Jobs / segments | Before, three listings | After, three listings |
| --- | ---: | ---: |
| 8 / 8,000 | 56.689 ms | 52.119 ms |
| 64 / 64,000 | 495.618 ms | 478.074 ms |

Baseline: `8c68679`. Retry counts now accumulate in the existing status aggregate,
eliminating a separate segments-table scan in both the individual summary and
all-job listing. The latter uses three queries instead of four. The measured
latency improvement is modest (about 4–8% here); the scan removal is the durable
change, and results will depend on database size and cache state.

No schema, index, migration, token-accounting policy, or persisted format changes.
Regressions cover retry counts across statuses and job boundaries, empty jobs,
unknown legacy statuses, and existing mixed legacy/ledger token accounting.
