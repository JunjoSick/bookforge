# Changelog

## Unreleased

Dogfooding round on a real 400-page-scale EPUB (English→Italian, DeepSeek):
fixes for every issue the live run surfaced.

- **Latency-aware batch budgets:** batch planning caps output tokens so the
  expected generation fits inside 80% of the request timeout, and the provider
  extends an individual request's deadline to cover its own output budget
  (never below the configured timeout, with a one-time log when it engages).
  Large single-segment batches no longer race a fixed 180s deadline. A
  slow-trickling-body regression test now proves the client timeout fires
  mid-body instead of hanging.
- **In-flight visibility:** `RequestProgress` heartbeats (every 5s per
  outstanding request) make long normal generations distinguishable from a
  stall, and `RequestStarted` gained an optional `effective_timeout_seconds`.
- **Structured, block-attributed QA findings:** deterministic findings persist
  with `kind`, instance severity, message, and `block_id` (migration 11).
  Source-copy hits on title/heading/short proper-noun blocks are warnings —
  leaving a book title or author line untranslated is editorially correct —
  while unchanged prose stays an error. Reports, `status`, and the review page
  prefer structured rows; legacy error strings decompose through a documented
  parser. The CLI no longer regex-parses engine error text.
- **Honest `estimate --pass-costs`:** per-pass surcharge lines plus a real
  `Estimated total incl. passes` (the old line printed the surcharge alone,
  less than the primary estimate). JSON events gained
  `est_cost_usd_passes`/`est_cost_usd_total`.
- **Supervised retries:** `retry` gains `--ui`, spawns a supervised
  replacement worker whose deaths are surfaced (`replacement_worker_died`
  events with exit status and stderr tail), respawn with exponential backoff,
  and bounded give-up that marks the job honestly. Root-caused and fixed the
  silent respawn loop from the dogfood run: the dashboard resume path held
  the runtime launch claim across child spawn, so every replacement worker
  bailed instantly on the claim check with its stderr discarded.
- **Friendly validation errors:** persisted error text no longer leaks raw
  serde internals ("missing field `translation` at line 1 column 157");
  categorized plain-language sentences instead, with raw detail in debug logs.
- `--no-thinking` and the provider connection flags gained real help text.

## v3.0.0 - 2026-08-27

Remediation campaign for the 2026-08 deep audit (`docs/report.md`, waves 0–4),
shipped as v3.0.0: security hardening around the local dashboard, reliability
repairs in the checkpoint/resume lifecycle, translation-quality work,
audio/PDF fixes, breaking JSON/CLI surface changes, and corrections to two
audit findings that turned out wrong. See `docs/HANDOFF-2026-08.md` for the
wave-by-wave record.

### Security

- **Security:** the browser dashboard authenticates by default. The server
  prints a one-time bootstrap URL containing its session token, and every
  route beyond that page requires the token in the `x-bookforge-csrf` header —
  closing the hole where any local process could harvest a token from `/` and
  spend remembered provider keys. `--no-auth` restores unauthenticated serving
  for consoles-only environments.
- **Security:** `.bookforge` roots are created private (0700 on Unix) from the
  first moment, pre-existing loose components are tightened at startup,
  dashboard-uploaded EPUBs are written 0600, and estimate-upload parsing uses
  unpredictable owner-only temporary directories that self-delete.
- **Security:** cancel requests check whether the stored worker PID is alive
  before any process action, so PID reuse can no longer kill unrelated trees.
- **Security:** job ids passed through dashboard routes are strictly
  allowlisted before reaching filesystem paths, blocking traversal-shaped
  reads of arbitrary files.
- **Security:** concurrent dashboard launches are capped, protecting against
  many parallel billable runs sharing remembered keys, and every launch runs
  behind a panic boundary so a crash cannot take the server down mid-run.
- **Security:** book text enters prompts inside fenced, sanitized context
  blocks instead of unfenced sections, shrinking the prompt-injection surface
  while validation still bounds structural damage.
- **Security:** archive reads for `reflow` and `validate` now go through the
  same decompression budget as translation parsing — no CLI command remains
  that an under-declared zip entry can OOM.
- Infrastructure hardening: CI gains a least-privilege permissions block,
  SHA-pinned actions, and a checksum-verified EPUBCheck download; zip is built
  deflate-only on the pure-Rust zlib-rs backend (drops the zstd-sys C
  toolchain requirement); explicit ignore rules cover test-key files and
  editor scratch directories.

### Reliability

- Human corrections are frozen by SQL inside single immediate transactions:
  `INSERT … WHERE human_corrected = 0` guarantees neither the CLI worker nor a
  dashboard job racing the same segment can overwrite an accepted correction,
  including during deliberate double-runs via `resume --force`.
- Each segment checkpoint commits as one transaction instead of three to five
  separate autocommits, removing crash windows between related writes.
- One malformed batch response can no longer abort a paid run. Failures
  carrying the phantom `"unknown"` segment id are re-attributed to the actual
  requested segment (or dropped with a warning when unattributable), and the
  checkpoint writer survives per-command errors: each poisoned command is
  logged as an error event and skipped, with a final
  `checkpoint_dropped_commands` warning totaling what was lost versus saved.
- Resume tells the truth again. Completion is decided from persisted segment
  statuses — not merely from rebuilt blocks — so jobs whose segments failed
  again during resume no longer report `succeeded`, and dead-row failures are
  surfaced. Hard command errors mark jobs `failed` instead of leaving them
  stuck `running`.
- Plain `resume` acquires a worker lease just like the dashboard, preventing
  two live workers from doubling provider spend on the same job.
- The pause/stop watcher owns one long-lived store connection instead of
  reopening and fully migrating SQLite roughly ten times per second for the
  life of every run.
- Interrupting matters everywhere: Ctrl+C during `resume` cancels cleanly, an
  attached TUI quit passes cancellation through, and late pause/stop requests
  can no longer rewrite terminal outcomes in the completion window.
- Exit codes are a stable documented taxonomy: `0` success/intentional stop,
  `1` runtime failure, `2` usage error, `3` finished-with-unresolved-segments,
  `130` interrupted. `doctor` exits non-zero on failed checks with `--no-fail`
  preserving green scripts.
- Store hardening: job and segment statuses gain typed enums at the boundary
  enforced by CHECK constraints (migration 10, defensive `Unknown` decode);
  the bundled SQL migration files become documented-and-parity-guarded rather
  than silently drifted; retention groundwork lands as the store API
  `JobStore::prune_jobs` (age/count/dry-run, running jobs protected per
  deletion; a user-facing command is planned separately); file hashing
  streams in chunks instead of reading whole EPUBs into RAM; startup sweeps
  empty `retry_pending_overrides_<pid>` directories whose owner process is
  gone.

### Quality and performance

- One canonical script-aware token estimator (`bookforge_core::token_estimate::estimate_tokens`)
  replaces eight divergent per-crate helpers (chars/4, bytes/4, words×4/3,
  dominant-case-class weighting) across batch packing, scheduler context
  budgets, glossary selection, QA and double-check chunking, the EPUB reader,
  provider mocks, and the judge examples. Unspaced-script characters (Han,
  Kana, Hangul) weigh one token each; everything else keeps the classic four
  characters per token, so mixed-script text is priced by proportion instead of
  a whole-text verdict. `CACHE_KEY_SCHEMA_VERSION` bumps to v3 and glossary
  fingerprint payloads to schema 2, invalidating cached translations produced
  under differently-sized segment groupings or glossary packing.
- Output-token budgets are honored uniformly: the effective ceiling is the
  smaller of the user cap and the context remainder, floors apply to net
  output, and batch and single-segment paths behave identically.
- Transient batch retries are paced with backoff and classify 408/425 as
  retryable; malformed JSON responses tolerate markdown fences and trailing
  prose, avoiding needless split/retry churn.
- Double-check concurrency settings are actually used; multiple correction
  rounds run for real, and applied corrections persist transactionally with a
  visible persistence marker before terminal events drain.
- Long prompts stopped shipping mojibake em-dashes; seven templates were
  repaired and a guard test keeps them clean.
- EPUB internals: script/style/svg/math content is suppressed as verbatim
  paired markers instead of being absorbed as translatable inline text;
  marker nesting is depth-capped; sixteen divergent helper implementations
  collapsed into one platform-neutral utility; validation matches extensions
  case-insensitively and checks each file once.
- Script-aware source-copy detection replaces the heuristic's Latin biases.
- Terminal surfaces sanitize external text, unify tri-state flag syntax,
  keep `--ui json` stdout free of human chatter, emit honest
  `DroppedEvents` records when events are lost, and rebaseline rate/ETA on
  resume epochs.
- Pricing loaders and provider/model defaults collapsed into one
  `bookforge_core::providers` registry (CLI wrappers, judge examples, and the
  glossary base-url table all consume it); bundled pricing JSON lives in a
  single package-owned copy instead of three divergent tree paths; `estimate`
  gains an optional `--pass-costs` breakdown covering QA/double-check/repair
  planning heuristics (`--double-check-passes`, `--repair-share`).
- Dead code removed after caller verification: unused core config/marker/
  entity/IR helpers, several CLI-only store/test plumbing pieces, PDF's dead
  image-type arms, and a wide slice of audiobook crate internals (public
  serde/report schemas untouched); the cache namespace gains its missing
  glossary domain separator.

### Audio

- A transient network failure during ElevenLabs auto-model selection now
  degrades deterministically to the cheapest suitable tier (never Eleven v3 or
  another premium tier) instead of failing open to Multilingual v2 pricing;
  the deterministic choice keeps resumed runs cache-compatible, and library
  consumers see `degraded: true` plus a reason.
- Audiobook output directories are locked cross-process while a build runs;
  concurrent invocations fail fast instead of corrupting manifests or pruning
  the other run's paid chunks, and stale locks left by dead processes are
  reclaimed automatically.
- Sentence chunking understands CJK punctuation (`。！？`) and stops cutting CJK
  prose mid-word and English sentences after abbreviations such as "Mr.".
- ffmpeg never blocks waiting for terminal input (`-nostdin`, null stdin),
  runs under timeouts, and is killed-and-reaped when hung; `--loudnorm`
  normalizes per-chapter intermediates so chapter markers track the published
  audio, and silently ignored options now warn loudly instead.
- `--prune` sweeps crash debris (interrupted `.part.tmp` writes, legacy
  `.replace.bak` backups, staged concat inputs) while never deleting managed
  outputs or lock files.

### PDF

- Temporary working directories are cleaned up through RAII even when figure
  or media passes fail early; successful OCR no longer wipes figure blocks
  anchored on the same pages; running-header removal is accounted for before
  coverage thresholds fire spurious OCR.
- Generated conversion artifacts gain deterministic content-hashed UIDs,
  respect `SOURCE_DATE_EPOCH`, and build their table of contents from the
  detected heading structure; caption detection warns when non-English labels
  will be missed; render sizes and OCR request bodies are capped.
- RTL lines (Arabic/Hebrew) emerge in logical reading order via a line-level
  bidi pass; dominant-RTL pages report `rtl_dominant` and stop skewing the
  coverage metric with embedding controls; justified-CJK kinsoku continuations
  merge across pages without invented spaces; caption detection now covers
  CJK typed prefixes (図/图/圖/表), a language-neutral lead-word fallback, and
  fullwidth ordinals — the foreign-caption warning fires only for numeral
  systems outside that repertoire.
- A PopplerBackend seam plus an in-process fake poppler drive the conversion
  test matrix on any OS, so env-scrubbing, timeouts, pipes, and cleanup claims
  no longer ship untested on Windows (TEST-2/PDF-2).

### Breaking-ish changes

Automation consumers: `--ui json` stdout now carries a versioned envelope
(UI-23). Every line is `{"v":2,"kind":"event"|"audiobook","payload":{…}}`:

- `translate --ui json` and `resume --ui json`: each line previously was a raw
  `ProgressEvent` object; it is now `kind:"event"` with that same object as
  `payload`. Update parsers to read `payload`.
- `audiobook --ui json`: each line previously was a raw `{"event":"…",…}`
  object; it is now `kind:"audiobook"` with that object unchanged as
  `payload`.
- Scripts pinned to the old shapes can pass `--ui json-v1`, a deprecated alias
  that reproduces the raw v1 streams byte-for-byte. The persisted
  `events.jsonl` file log, `tail <job-id> --json`, and dashboard SSE frames are
  **unchanged**. See `docs/events.md` for the full contract and stream table.
- Rendering consolidation (UI-31): all four dashboards (TUI, progress bars,
  `tail` reconstruction, serve folds) now share one RunState+EpochTracker view
  and formatter set. Visible nits: an ETA of zero/unknown renders as `—`
  instead of `0s` in the bars' rate line, and `tail`'s human reconstruction
  block gained a `status:` row using the shared status vocabulary. Counts,
  rates, and ETA values were already cross-checked by the wave-2 epoch tests
  and remain identical otherwise.
- `glossary clear`, `style clear`, and `entities clear` now require an
  explicit `--yes` before they destroy data.
- `doctor` exits non-zero when health checks fail (see Reliability) — scripts
  that treated a failed check as green need `--no-fail`.
- The dashboard authenticates by default (see Security): scripts driving the
  loopback API without the session token receive `401`. `--no-auth` opts out.
- The `bookforge-audio` crate's internal surface was trimmed: narration text
  helpers (`Chapter::text`, `chunk_text`, `chapters_from_book`), ElevenLabs
  constants/`fetch_elevenlabs_subscription_with_key`/`resolve_preferred_elevenlabs_model`
  (superseded by `*_reported_with_cancel` variants), and
  `single_file_ffmpeg_args` are no longer public. The serde report/manifest
  schemas are unchanged.

### Audit corrections

- PDF-1 was refuted: `cargo test -p bookforge-pdf` compiles and runs on
  Windows, and the ungated tests touch none of the cfg(unix) helpers.
- PDF-4 was refuted with poppler 26.08 evidence: adding `-i` to pdftohtml
  strips the image-placement tags the converter relies on, so the flag stays
  absent deliberately.

## v2.6.1 - 2026-07-21

- **Security:** ElevenLabs keys are no longer passed to the quota lookup through
  a process environment variable. Setting `set_var` in a multi-threaded program
  is unsound on Unix and let concurrently spawned children inherit the key.
- **Security:** EPUB decompression is bounded by entry count, per-entry and total
  uncompressed size, and compression ratio, enforced during the read so an entry
  that understates its size in the header cannot expand unchecked.
- **Security:** poppler invocations time out, have their output capped, and run
  with a cleared environment plus a narrow allowlist, so a compromised tool no
  longer sees every provider key. Temporary directories are randomized and 0700.
- **Security:** provider responses, OCR responses and event-log lines are read
  against explicit byte ceilings.
- **Security:** `.bookforge` directories are created 0700 and EPUB snapshots 0600
  on Unix, applied at creation.
- **Security:** dashboard-spawned jobs receive only the provider key they need,
  and dashboard responses carry CSP, `X-Frame-Options`, `nosniff`,
  `Referrer-Policy` and `Cache-Control: no-store`. The Google Fonts dependency is
  gone in favour of a system font stack.
- **Security:** release workflow actions are pinned to commit SHAs via
  `dist-workspace.toml`, so the pins survive workflow regeneration.
- Fixed `eleven_v3` audiobook runs failing with `unsupported_model`: the request
  carried `previous_text`/`next_text`, which that model rejects.
- Fixed the ElevenLabs quota preflight comparing raw characters against a balance
  denominated in credits, which warned about runs that comfortably fit.

## v2.6.0 - 2026-07-21

- **Breaking:** existing audio chunk caches are invalidated by the
  `bookforge-audio-v2` synthesis hash and manifest schema 3. The manifest adds
  narration kind, seed, language, text-normalization, gaps, and author data
  while retaining serde-defaulted backward reads. Rerun audiobook jobs with
  `--prune` to remove superseded chunk files (`--prune --dry-run` previews the
  cleanup).
- Audiobook narration now preserves document hierarchy: chapter titles and
  in-section headings are emitted as separate typed chunks instead of being
  packed into body prose. Stitching adds configurable chapter, title/heading,
  and paragraph silence (1200/800/0 ms by default), with automatic ElevenLabs
  `<break>` tags on supported v2 models.
- ElevenLabs synthesis now supplies up to 300 characters of same-chapter
  `previous_text` and `next_text` for smoother joins, plus optional `--seed`,
  `--language`, and `--text-normalization` controls. Language codes are sent
  only to Flash/Turbo v2.5 and are warned-and-dropped for every other model or
  provider.
- A chaptered `audiobook.m4b` with chapter markers and title/artist metadata is
  now the default deliverable when ffmpeg is available; `--no-book-file` opts
  out and `--single` adds a flat audio file for on-the-go listening. Added
  configurable whole-book `--loudnorm` while leaving per-chapter files
  unnormalized.
- Added schema-1 per-character TTS pricing in
  `crates/bookforge-cli/pricing/audio-providers.json`, estimated dollar/credit
  cost in audiobook plan and dry-run output, and a non-fatal ElevenLabs
  subscription-quota preflight. Pricing figures are estimates; provider billing
  is authoritative. Added one-based chapter subset selection with `--chapters` and
  ElevenLabs account voice discovery with `--list-voices`.
- Brought the browser audiobook workflow to CLI parity: ElevenLabs now offers
  `Auto (recommended)` model selection and a server-side voice-list proxy at
  `GET /api/audio/voices`; pre-launch estimates from
  `POST /api/audiobook/estimate` include cost and quota; progress is grouped by
  chapter; and completed `.m4b` files can be played in-page. An Advanced
  section exposes chapter pause, flat-file output, loudness normalization,
  seed, and language.
- Added optional OCR recovery for low-confidence PDF pages through
  OpenAI-compatible endpoints, including the SGLang-specific Unlimited-OCR
  dialect, retrying blocking HTTP client, `action=ocr` reporting, and a
  loopback-friendly endpoint doctor.
- Added an Unlimited-OCR deployment guide and helper script for serializing
  SGLang's custom no-repeat n-gram logit processor.
- Audiobook chapter extraction now recognizes canonical Toki Pona headings
  such as `lipu nanpa VI` and can recover chapter boundaries from legacy
  BookForge translations that used the old `KAPITELO` label.
- ElevenLabs audiobook requests now enforce each model's documented character
  limit consistently in the CLI, dashboard, server, and provider layer: 40,000
  for Flash/Turbo v2.5, 10,000 for Multilingual v2, and 5,000 for Eleven v3.
- ElevenLabs live audiobook runs now auto-select the first available compatible
  model in the preference order Eleven v3, Flash v2.5, Turbo v2.5, then
  Multilingual v2, with a warning and Multilingual v2 fallback if preflight
  fails. Explicit models still bypass preflight, and dry runs stay offline.
- ElevenLabs requests now omit `voice_settings` at the default speed, reject
  speed changes with Eleven v3, and list dashboard models in preference order
  while retaining Multilingual v2 as the dashboard default.

## v2.5.1 - 2026-07-16

- Fixed drag-and-drop uploads in the dashboard. The translation and audiobook
  "drop" zones looked droppable but only responded to clicks — dragging an
  EPUB onto them did nothing. They now handle drag-and-drop, validate the
  `.epub` extension, and highlight while a file is dragged over them.
- Fixed "Access is denied. (os error 5)" when starting a translation from the
  dashboard. The server and the runs it spawns persist state under a
  `.bookforge` directory resolved relative to the process working directory.
  Launched from the desktop shell (a Start Menu shortcut, an installer, or a
  URL/protocol handler), that directory is often read-only (for example
  `System32` or `Program Files`), so the first upload write failed. `bookforge
  serve` now relocates to a per-user data directory (`%LOCALAPPDATA%\BookForge`
  on Windows, `$XDG_DATA_HOME`/`$HOME` elsewhere) when the working directory
  is not writable, and leaves a writable working directory untouched.

## v2.5.0 - 2026-07-15

- Added audiobook generation: `bookforge audiobook <book.epub>` narrates an
  EPUB chapter by chapter through any OpenAI-compatible text-to-speech
  endpoint (OpenAI, or a local server such as kokoro-fastapi via
  `--base-url`). Deterministic Rust owns structure — chapter extraction that
  skips the OPF metadata and table of contents, sentence-boundary chunking
  under the provider character limit, atomic per-chunk writes, file-based
  resume keyed by content and synthesis settings, and a JSON manifest — while
  the provider only ever sees a text chunk and returns audio bytes. The hosted
  default is `gpt-4o-mini-tts`, with optional pronunciation/delivery
  instructions. Optional `--stitch`/`--m4b` join the parts with
  ffmpeg (with chapter markers when ffprobe is present), degrading gracefully
  when ffmpeg is absent. A deterministic mock provider backs `--dry-run` and
  the offline test suite. New `bookforge-audio` crate. See
  [docs/audiobooks.md](docs/audiobooks.md).
- Added native Gemini and ElevenLabs audiobook providers. Gemini Generate
  Content TTS decodes its 24 kHz PCM response into WAV by default and supports
  prompted delivery guidance. ElevenLabs uses its voice-ID path, `xi-api-key`
  authentication, native `model_id` payload, and MP3/Opus/WAV/PCM formats.
- Added `bookforge audiobook --prune` to remove audio chunk files left over
  from earlier runs (a different voice, model, speed, format, or edited source
  text) that the current plan no longer uses. It never deletes the current
  run's chunks, the stitched per-chapter outputs, the assembled `.m4b`, or the
  manifest, and `--prune --dry-run` reports what would be removed without
  deleting anything.
- Added Toki Pona as a first-class translation target: selecting it
  automatically activates a tested minimalist-language translation contract,
  it is offered in the dashboard language list, and the repo ships an editable style sheet
  (`docs/styles/toki-pona.style.toml`) plus a run guide
  ([docs/toki-pona.md](docs/toki-pona.md)) covering model choice, name
  tokiponization, and why suspicious-mode QA is a poor fit for a ~120-word
  target language.
- Added matching audiobook workflows to the full-screen terminal UI and local
  browser dashboard. Audiobooks can be generated directly from any source
  EPUB; translation is optional.
- Hardened EPUB rebuilding so source-language chapter labels and nested NCX
  navigation remain intact, and made ordinary EPUB reading lossless by moving
  PDF-derived cleanup behind the explicit reflow command.
- Added a conservative Toki Pona profile with 200-token, single-item
  requests, marker-safe fallback, and validation that rejects suspect output
  instead of semantically rewriting it.
- Added durable audiobook operation manifests, real in-flight request
  telemetry, provider payload/signature validation, redirect-safe API-key
  handling, corrupt-cache recovery, and retry/backoff improvements.

## v2.4.1 - 2026-07-14

BookForge v2.4.1 is a security-gate maintenance release.

- Replaced the yanked transitive `spin 0.9.8` lockfile entry with the
  compatible, non-yanked `spin 0.9.9` release.
- Granted the RustSec workflow the minimal Checks API permission it needs to
  report audit results without turning dependency warnings into workflow
  failures.
- Added GitHub/Sigstore provenance attestations for release assets so users
  can verify that downloads were produced by BookForge's release workflow.
- Hardened the lifecycle release gate against child-process scheduling delays
  on loaded Linux CI runners.

## v2.4.0 - 2026-07-13

BookForge v2.4.0 closes the local review loop and makes safe runtime tuning
genuinely live while strengthening the release gates around both changes.

- Added revisioned, atomic live job reconfiguration through the CLI and the
  dashboard. Cache-safe concurrency, request-budget, retry, adaptive sizing,
  QA, double-check, and validation settings now apply at explicit request,
  batch, or finalize-stage boundaries without changing in-flight requests or
  retranslating completed segments.
- Added worker leases and lease-aware dashboard controls: Resume signals a
  live worker or safely launches one replacement for a stopped, crashed, or
  dead-paused job, with concurrent launch attempts deduplicated.
- Added durable, auditable human corrections and flags to the CLI and Review
  dashboard. Corrected segments are frozen against model/QA/cache overwrites,
  guided retries persist across resume, and retry guidance now uses an inline
  editor with explicit Stop/Resume workflow.
- Added Windows workspace CI, Dependabot coverage, RustSec auditing, and Rust
  CodeQL analysis; the new workflows have been exercised on the v2.4 draft PR.
- Added escalation-first handling for truncated batch responses and prominent
  systemic-truncation alerts across CLI, watch, and dashboard surfaces.
- Fixed resume override ordering, adaptive batch override propagation, stale
  override cleanup, stop/resume lifecycle timing races (including scheduler-
  delayed CI child startup), and dashboard wizard API-key retention.
- Bumped the batch translate prompts (plain, marker-safe, run-preserving, and
  their compact variants) from v2 to v3 to teach models about a per-item
  `retry_guidance` field; the batch prompt cache tag moved from `batch_v2`
  to `batch_v3` so translations cached under the old prompt text are not
  reused.
- Split the largest dashboard, batch, store, PDF-conversion, and translate
  modules behind behavior-preserving seams while retaining the embedded
  single-binary dashboard and existing public APIs.
- Added an explicit pre-v2.4 database migration regression proving existing
  translations and blocks survive the new human-correction audit columns.
- Exercised the generated installers on native x86_64/aarch64 macOS and Linux
  plus Windows MSVC, and completed a selected DeepSeek pause, live-reconfigure,
  correction, guided-retry, replacement-worker, persistence, and output-
  validation acceptance run.

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
