# Architecture

Bookforge is an EPUB-first translation engine. The program owns document structure; language models translate prose segments only.

## Crates

- `bookforge-core` defines the IR, segmentation, run settings, progress events, cache namespace rules, glossary/style/entity helpers, and marker parsing.
- `bookforge-pdf` owns Poppler-backed PDF layout extraction, deterministic reconstruction, optional OCR recovery, synthetic EPUB assembly, and fidelity reports.
- `bookforge-epub` reads EPUB containers into the IR and rebuilds EPUBs by patching original XML resources in place.
- `bookforge-llm` owns prompt rendering, provider calls, batching, validation, repair, QA, context registries, and telemetry.
- `bookforge-store` owns the SQLite job store, segment records, block translations, cache lookup, retry state, and run snapshots.
- `bookforge-audio` owns chapter extraction, typed narration chunking, TTS providers, resumable synthesis, stale-chunk cleanup, and optional ffmpeg stitching.
- `bookforge-cli` wires those pieces into commands such as `translate`, `resume`, `retry`, `review`, `inspect`, `validate`, `status`, `convert`, `audiobook`, and `serve`.

## Translation Flow

1. `translate` resolves the profile and provider settings. The default profile is the fast v1 profile, which enables batching and adaptive sizing.
2. The EPUB reader parses the package document, NCX files, spine XHTML, XHTML head titles, and visible body text into stable sections and blocks.
3. Segmentation groups blocks into bounded translation units. Code blocks are retained in the IR but skipped for translation.
4. The CLI creates a job, snapshots the input EPUB and resolved run settings, inserts segment records, and scans the cache.
5. `commands/translate/engine.rs` runs the shared checkpointed execution path used by both fresh translation and resume. It selects batch or single-segment execution, starts the checkpoint writer, and persists each validated segment as it completes.
6. Post-processing can run QA, fallback, and double-check passes depending on settings.
7. The EPUB writer patches translated blocks back into the original resources and writes a report next to the output.

The key invariant is that models never produce EPUB structure. They produce JSON translations of prose strings. Rust validates the response, maps marker-protected prose back to known blocks, and patches the original package resources.

## PDF Conversion Flow

1. The `convert` command uses Poppler's `pdftohtml -xml` output as the raw layout and `pdftotext` as the comparison baseline.
2. `parse.rs` turns the XML into pages of positioned fragments and styled spans.
3. `reconstruct.rs` merges fragments into lines, detects columns and reading order, clusters paragraphs, repairs line-end hyphenation, and derives headings from font sizes.
4. When an OCR endpoint is configured, `ocr.rs` uses an `OcrEngine` and `OcrDialect` to recover low-confidence pages through an OpenAI-compatible API.
5. `epub.rs` writes the synthetic EPUB. `report.rs` compares reconstruction with the `pdftotext` baseline and records per-page column decisions, low-confidence actions, media counts, and layout warnings.

The produced EPUB then flows through the ordinary BookForge pipeline. The PDF crate is an ingestion front end, not a parallel translation path.

## Audiobook Flow

1. `text.rs` turns a parsed `bookforge_core::ir::Book` into chapters and typed title, heading, and body narration chunks. It splits within the configured provider character limit at sentence boundaries where possible.
2. `provider.rs` and `provider/` define `TtsProvider` and provide OpenAI-compatible, Gemini, ElevenLabs, and deterministic mock providers.
3. `builder.rs` orchestrates book to chunks to audio files with bounded concurrency, atomic per-chunk writes, content-and-settings-hashed file-based resume, and a JSON manifest.
4. The `--prune` path uses `cleanup.rs` to identify and remove managed chunk files that the current plan no longer uses.
5. `stitch.rs` optionally joins chunks per chapter and assembles a chaptered `.m4b` through `ffmpeg`. If `ffmpeg` is absent, it warns and leaves the per-chunk files intact.

The same invariant applies as in translation: deterministic Rust owns chapter extraction, chunking, file layout, and resume. Providers receive plain text chunks and return audio bytes.

