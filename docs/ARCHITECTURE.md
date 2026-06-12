# Architecture

Bookforge is an EPUB-first translation engine. The program owns document structure; language models translate prose segments only.

## Crates

- `bookforge-core` defines the IR, segmentation, run settings, progress events, cache namespace rules, glossary/style/entity helpers, and marker parsing.
- `bookforge-epub` reads EPUB containers into the IR and rebuilds EPUBs by patching original XML resources in place.
- `bookforge-llm` owns prompt rendering, provider calls, batching, validation, repair, QA, context registries, and telemetry.
- `bookforge-store` owns the SQLite job store, segment records, block translations, cache lookup, retry state, and run snapshots.
- `bookforge-cli` wires those pieces into commands such as `translate`, `resume`, `retry`, `review`, `inspect`, `validate`, and `status`.

## Translation Flow

1. `translate` resolves the profile and provider settings. The default profile is the fast v1 profile, which enables batching and adaptive sizing.
2. The EPUB reader parses the package document, NCX files, spine XHTML, XHTML head titles, and visible body text into stable sections and blocks.
3. Segmentation groups blocks into bounded translation units. Code blocks are retained in the IR but skipped for translation.
4. The CLI creates a job, snapshots the input EPUB and resolved run settings, inserts segment records, and scans the cache.
5. `commands/translate/engine.rs` runs the shared checkpointed execution path used by both fresh translation and resume. It selects batch or single-segment execution, starts the checkpoint writer, and persists each validated segment as it completes.
6. Post-processing can run QA, fallback, and double-check passes depending on settings.
7. The EPUB writer patches translated blocks back into the original resources and writes a report next to the output.

The key invariant is that models never produce EPUB structure. They produce JSON translations of prose strings. Rust validates the response, maps marker-protected prose back to known blocks, and patches the original package resources.

