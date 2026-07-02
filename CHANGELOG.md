# Changelog

## v2.0.2 - 2026-07-02

BookForge v2.0.2 corrects the non-technical install commands so they point to
the installer asset names produced by the release workflow.

- macOS/Linux documentation now downloads `bookforge-cli-installer.sh`.
- Windows documentation now downloads `bookforge-cli-installer.ps1`.

Validation:

- `cargo metadata --no-deps --format-version 1`
- `cargo fmt --all --check`
- `git diff --check`

## v2.0.1 - 2026-07-02

BookForge v2.0.1 is a usability patch for the new local browser workflow.
It makes the release binary friendlier for non-technical users who do not have
Rust or Cargo installed.

- Running `bookforge` with no subcommand now opens the local browser dashboard
  in default release builds.
- The README install guide now leads with prebuilt installers and plain
  `bookforge` startup instructions instead of Cargo-oriented setup.
- Minimal `--no-default-features` builds still omit the dashboard and print CLI
  help when run without a subcommand.

Validation:

- `cargo fmt --all --check`
- `git diff --check`
- `cargo test -p bookforge-cli --features serve`
- `cargo test -p bookforge-cli --no-default-features`
- `cargo clippy --workspace --all-targets --all-features -- -A clippy::too_many_arguments -D warnings`
- `cargo run -p bookforge-cli --features serve -- --help`

## v1.8.5 - 2026-06-29

BookForge v1.8.5 is a hardening release for translation reliability,
validation accuracy, EPUB rebuild coverage, checkpoint reporting, and PDF
conversion safety.

- LLM batch translation now caps transient retries, preserves document block
  order during finalization, rejects unknown or duplicate run IDs, and keeps
  provider-level failures out of item repair.
- QA and double-check passes now preserve non-corrective warnings, report
  provider-omitted audit items, and use the QA batch prompt with the configured
  batch token budget.
- EPUB ingestion/rebuild now handles non-spine EPUB3 navigation documents,
  start/end OPF manifest items, normalized/percent-decoded OPF hrefs, and
  target-language `dc:language` updates.
- EPUB validation now performs marker and protected-span checks once per
  translated block instead of duplicating them for every XML file.
- CLI lifecycle handling now fails unsupported providers correctly, writes
  failed reports after strict validation failures, emits checkpoint progress
  after persistence, counts all terminal segment states, and keeps queued jobs
  in `needs_review`.
- Style/entity global upserts and scoped clear commands now handle NULL/global
  scopes correctly.
- PDF conversion now obtains the `pdftotext` baseline before writing the EPUB,
  avoiding orphan output files when baseline extraction fails.

Validation:

- `cargo test --workspace`
- `cargo fmt --check`
- `git diff --check`
- `cargo clippy --workspace --all-targets --all-features`

## v1.8.1 - 2026-06-22

BookForge v1.8.1 hardens translation-response validation after full-book
DeepSeek testing exposed malformed markers and untranslated prose that could
otherwise reach a checkpoint and be preserved by the EPUB writer.

### Fixes

- Validate each run-preserving batch item after its runs are joined, rejecting
  malformed, missing, duplicated, or unknown inline markers before
  checkpointing.
- Use block-local markers throughout batch construction and double-checking
  instead of requiring markers that belong to sibling blocks.
- Reject long unchanged or nearly unchanged source-language prose in normal
  batch, turbo, and non-batch translation paths, while exempting
  notes/bibliography-style sections and same-language/mock runs.
- Include cached translations in double-check audits and match source blocks
  by stable block ID instead of array position.
- Route deterministic untranslated-copy findings directly to correction, use
  smaller correction batches, and reject malformed-marker or unchanged
  corrections.

### Validation

- Full locked workspace tests, formatting, clippy, and release build.
- Package contents verified for all six crates; the `bookforge-core` package
  also completed registry-resolution and compile verification.
- EPUBCheck 5.3.0 and BookForge validation of both full EPUB fixtures.
- Structural and text-coverage verification of the available PDF fixture.

## v1.8.0 - 2026-06-20

BookForge v1.8 adds the structural-credibility layer planned in ROADMAP
section 8.

### Highlights

- Added EPUBCheck-backed `validate` reports with structured messages,
  graceful unavailable-tool handling, and strict warning mode.
- Added `translate --validate-output [--strict-epubcheck]`.
- Added a pinned nine-book Standard Ebooks corpus, fetch/smoke tooling,
  pull-request coverage, and nightly/manual full-corpus CI.
- Added `local-ollama` and `local-llamacpp` presets plus `/models`
  health checks.
- Moved provider/model prices to bundled JSON with `--pricing` and
  `BOOKFORGE_PRICING_PATH` overrides.
- Kept the compile-time pricing copy inside the CLI crate so crates.io
  packages contain the same bundled catalog as workspace builds.
- Rewrote the README around the deterministic structure boundary and
  documented corpus and local-model workflows.
- Fixed navigation-list translation so labels stay inside EPUB nav links.
- Made PDF-converted EPUBs EPUB 3 conforming with a nav document and
  `dcterms:modified` metadata.
- Accounted for per-item JSON overhead in batch output budgets and enabled
  extended DeepSeek output budgets, reducing avoidable truncation/splitting.
- Disabled DeepSeek thinking mode in translation presets to avoid spending
  output budget and latency on hidden reasoning.
- Stopped provider doctor output from revealing partial API-key characters.
- Staged EPUB rebuilds beside the destination and swapped them into place only
  after ZIP finalization, including safe in-place source replacement.
- Added balanced/mis-nested marker validation before checkpointing so a job
  cannot report success while the writer silently preserves source blocks.

## v1.7.0 - 2026-06-20

BookForge v1.7.0 aligns the workspace packages, CLI version, and GitHub
release metadata on the current application version. It also publishes
the latest roadmap, repository ignore rules, and project handoff notes.

The executable feature set remains the extraction, scheduling, and
initial poppler-based PDF-to-EPUB conversion work already present on
`main`; bilingual output remains planned and is not claimed by this
release.

### Validation

- `cargo fmt --all --check`
- `RUSTFLAGS="-D warnings" cargo test --workspace --locked`
- `RUSTFLAGS="-D warnings" cargo clippy --all-targets --all-features -- -A clippy::too_many_arguments -D warnings`
- `RUSTFLAGS="-D warnings" cargo build --release --locked`

## v1.5.0 - 2026-06-13

BookForge v1.5 is the extraction and scheduling hardening release. It
focuses on making EPUB translation safer before spending real provider
tokens. The GitHub `main` release base also includes the first committed
PDF ingestion slice: poppler-based PDF-to-EPUB conversion P0/P1.

### Highlights

- Added text-coverage reporting to `inspect`, including per-file
  low-coverage diagnostics for visible text that would not be translated.
- Captured more real EPUB text shapes: bare text in common containers,
  OPF titles, NCX TOC labels, XHTML head titles, named HTML entities,
  nested same-name blocks, and text outside the original block whitelist.
- Kept `pre` and `code` blocks out of translation so source whitespace is
  preserved byte-for-byte.
- Replaced long inline marker tags with short per-block markers and moved
  translation prompts to the v2 prompt contract.
- Changed missing protected spans from silent append-repair into explicit
  validation failures that fall back to source text and `needs_review`.
- Made sliding context best-effort by default to avoid serializing or
  deadlocking long runs; `--context-strict` restores the old completion
  fence when needed.
- Shared the checkpointed run engine between `translate` and `resume`,
  with expanded lifecycle tests using synthetic EPUB fixtures.
- Made `v1-fast` the default translation profile.
- Added `bookforge convert` for initial poppler-based PDF text
  extraction and synthetic EPUB assembly.

### Validation

- `cargo fmt --all --check`
- `RUSTFLAGS="-D warnings" cargo test --workspace --locked`
- `RUSTFLAGS="-D warnings" cargo clippy --all-targets --all-features -- -A clippy::too_many_arguments -D warnings`
- `RUSTFLAGS="-D warnings" cargo build --release --locked`

The local Rust 1.88.0 MSRV check and crates.io packaging verification
were not run in this environment because external download approval was
blocked earlier.
