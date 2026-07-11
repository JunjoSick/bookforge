# Changelog

## Unreleased (2.4.0-dev)

BookForge main now carries a distinct development version so source builds
cannot be confused with the published v2.3.0 artifacts.

- Added cache-safe job reconfiguration through `bookforge reconfigure`, with
  auditable overrides that apply when a stopped or dead paused job resumes.
- Added escalation-first handling for truncated batch responses and prominent
  systemic-truncation alerts across CLI, watch, and dashboard surfaces.
- Fixed resume override handling, adaptive batch override propagation, stale
  override cleanup, and dashboard wizard API-key retention.
- Bumped the batch translate prompts (plain, marker-safe, run-preserving, and
  their compact variants) from v2 to v3 to teach models about a per-item
  `retry_guidance` field; the batch prompt cache tag moved from `batch_v2`
  to `batch_v3` so translations cached under the old prompt text are not
  reused.

## v2.3.0 - 2026-07-05

BookForge v2.3.0 added source-EPUB reflow and cooperative job controls.

- Added `bookforge reflow`, including the opt-in `--aggressive` level, while
  preserving the rule that source EPUBs are never silently rewritten during
  translation.
- Added pause, resume, and stop controls to the engine, CLI, terminal watcher,
  and browser dashboard, with completed segments checkpointed before parking.
- Made finalize-stage QA and double-check requests honor pause/stop controls;
  stopped jobs remain resumable without retranslating completed segments.
- Hardened reflow boundaries around letterless paragraphs, inline whitespace,
  dehyphenation, and class-based merge guards.

## v2.2.0 - 2026-07-04

BookForge v2.2.0 ships the v1.7 roadmap milestone: bilingual output.
A new `--mode` flag produces bilingual EPUBs with the translation
appended after the original instead of replacing it — for language
learners, bilingual readers, and verification reading.

- `--mode replace|append-text|append-block` (default `replace`,
  unchanged behavior). `append-block` adds a sibling
  `<p class="bookforge-translation" lang="…">` after each source block;
  `append-text` adds one inline span at the end of each block's content,
  separated by `--bilingual-separator` (default " / ").
- Per-element append policy keeps output EPUB-valid: nested paragraphs
  inside list items, table cells, and captions; heading translations as
  styled paragraphs; code/pre untouched; block markup inside translation
  wrappers is flattened.
- Bundled stylesheet (`--bilingual-style minimal|prominent|inline-only`)
  or custom `--bilingual-css`; CJK targets skip italics; RTL targets
  (ar/he/fa) get `dir="rtl"`; unmappable language names fall back to
  the `und` language tag.
- The target language is recorded as a secondary `<dc:language>`; the
  table of contents stays source-language in append modes.
- Bilingual mode is a pure reassembly concern: same prompts, same
  single-target translation contract, and the segment cache is shared
  across modes — switching modes reuses every cached translation.
- Resume/retry remember the mode; pre-v2.2 jobs resume unchanged.

Validation: full workspace suite (476+ tests), mock-provider end-to-end
in both append modes on a real converted book with structural validation
and manual inspection of the output XHTML.

## v2.1.0 - 2026-07-03

BookForge v2.1.0 ships the v1.6 roadmap milestone: PDF ingestion hardening.
Scientific papers and unorthodox-layout books (landscape art books, zine
formats) now convert to translation-ready EPUBs with figures, tables, and
equations preserved as correctly-cropped images.

- Figure/table/equation regions are detected and preserved as page crops,
  with crop coordinates correctly scaled between pdftohtml XML units and the
  render DPI (pdftohtml zoom is now pinned explicitly).
- Extracted raster sub-images cluster into one composite figure per diagram;
  decorative/repeated rasters are filtered and reported instead of emitted.
- Media regions are mutually exclusive (figures over tables over equations),
  detectors skip fragments inside image regions, and vector-figure/table
  regions clamp to their column on two-column pages — prose is never absorbed
  into an image region.
- pdfimages mask/stencil rows no longer shift page attribution; extracted
  images pair with layout regions by dimensions rather than position.
- pdfimages/pdftoppm are optional: without them conversion degrades to
  text-only with a warning instead of failing (`doctor` lists them as
  recommended).
- Low-confidence pages fall back per `--low-confidence` (linearize/preserve);
  coverage reporting credits media-preserved characters separately so
  preserved figures never read as text loss.
- Layout audit warnings itemize every text fragment absorbed into a media
  crop and flag paragraph joins around media blocks for review.
- Shared math-symbol classification moved to `bookforge-core`, used by both
  PDF equation detection and EPUB inline-math protection.

Validated end-to-end with real poppler against a two-column scientific paper
(arXiv 1810.04805) and two unorthodox-layout books (a 213-page landscape
InDesign art book at 99.3% text coverage and a small-format zine at 100%),
including full EN→IT translation runs on the converted output.

## v2.0.3 - 2026-07-02

BookForge v2.0.3 hardens the local browser dashboard, fixes inline-marker
spacing in translated EPUBs, and cleans up release quality gates for the v2
line.

- Dashboard requests now validate the `Host` header against the loopback host
  and bound port, closing DNS-rebinding access to the unauthenticated local UI.
- Browser-launched `openai-compatible` runs now require an `https://` base URL,
  CSRF token comparison no longer uses direct string equality, and detached
  translation launches get a short early-exit check before the dashboard reports
  success.
- The dashboard library, wizard, and progress grids now adapt to narrow windows,
  including a mobile breakpoint that stacks the wizard rail above the panel.
- EPUB extraction now keeps whitespace-only text nodes between adjacent inline
  elements as their own marker-boundary runs, and EPUB rebuild re-inserts a
  deterministic space when a model strips whitespace at an originally spaced
  inline boundary.
- CLI help/error printing now treats broken pipes as clean exits, including
  minimal builds that print help when run without the dashboard feature.
- Established long-argument helper APIs now carry targeted clippy allows so
  the exact workspace clippy gate is quiet.
- `docs/ROADMAP.md` now marks v2.0 as shipped and points readers to current
  docs for live behavior.

Validation:

- `cargo check -p bookforge-cli --all-features`
- `cargo test -p bookforge-cli`
- `cargo test -p bookforge-epub`
- `cargo check --workspace --all-features`
- `cargo test --workspace`
- `cargo clippy --workspace --all-features`
- `cargo fmt --check`
- `git diff --check`
- `cargo run -p bookforge-cli --features serve -- serve --bind 127.0.0.1:0`

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
