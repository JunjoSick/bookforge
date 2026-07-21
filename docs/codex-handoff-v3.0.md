# BookForge v3.0 — Codex handoff: audiobook narration quality + WebUI parity

**Status:** in progress. **Branch:** `feat/v3.0-audiobook` (stacked on `codex/audiobook-provider-and-chapter-fixes` / PR #40, with `main` merged in).
**Worktree:** `tmp/worktrees/v3.0` — all v3.0 work happens there; `main`'s working tree carries unrelated uncommitted work and must not be touched.

**How to use this document:** each `W#` brief below is a self-contained Codex task. Dispatch with
`codex exec --full-auto "$(cat docs/codex-handoff-v3.0.md)"` scoped to one brief, or paste the brief plus the *Shared context* section. Workers never commit; the orchestrator reviews every diff, runs verification, and owns all live-credit spend.

**Execution order (dependency-driven — `bookforge-cli` will not compile until W1 lands):**
| Phase | Workers | Why |
|---|---|---|
| 1 | **W1** (bookforge-audio) ∥ **W3** (docs) | W3 touches no code, so it parallelizes safely |
| 2 | **W2** (serve parity) ∥ **W4** (CLI wiring) | disjoint files; both need W1's crate to compile |
| 3 | **W5** (serve knobs + byte literals) | same files as W2, needs W4's flags |
| 4 | orchestrator | full test sweep, live checkpoints, release prep |

## Context

Today's live E2E (the *Che fare* audiobook) exposed the limitations the user wants fixed for v3.0:

1. **No narration hierarchy.** A chapter title is narrated as flat prose. Root cause: the section's first heading block produces `Chapter.title` *and* is emitted as the first paragraph of `Chapter.text`; `chunk_text` (`text.rs:400-440`) then packs that heading into the same chunk as the following body sentences, replacing the `\n\n` break with a single space. In-section h2–h6 headings are likewise flattened. The TTS request carries zero signal that a span is a title.
2. **No true single-file output.** Only a chaptered `.m4b` exists, it requires an explicit flag, and it has no title/author metadata. No flat mp3.
3. **No prosody consistency.** Chunks are synthesized in isolation (`builder.rs:310-400`); neighbor text is knowable from the ordered plan but never threaded. ElevenLabs' continuity fields (`previous_text`, `next_text`, `seed`, `language_code`) are all dropped — the body sends only `{text, model_id, voice_settings}`.
4. **Missing polish:** no loudness normalization, no inter-chapter/heading silence, no cost/quota estimate, no chapter-subset selection, no voice listing.
5. **WebUI lags the CLI** — and it's the surface the user's partner actually uses: no auto-model, no voice picker, no playback, no cost/quota preview, none of the new knobs.

**Goal:** v3.0 makes the audiobook *sound* like an audiobook (title/heading separation, consistent voice, clean joins), consolidates output, and brings all of it to the WebUI first.

**User decisions (locked):**
- **Output:** chaptered file is the **default** deliverable (`.m4b` with chapter markers + title/author metadata, auto-enabled when ffmpeg is present). A **flat single mp3** is opt-in (`--single`, "flat mp3 for on-the-go" in the UI).
- **Live testing:** **multiple small live ElevenLabs checkpoints** — one tiny smoke after each of WP-A, WP-B, WP-C. Budget-conscious: ~15k credits remain, so each smoke uses `eleven_flash_v2_5` and only the ~2k-char *Correzione* chapter via `--chapters`. Hard stop at ≤90% of remaining.

---

## Shared context — paste into EVERY worker brief

**Repo:** `C:\Users\gangi\Desktop\bookforge` (Rust workspace, 7 crates, edition 2024).
**Branch:** `codex/audiobook-provider-and-chapter-fixes` — **check it out first**; the working tree may be on `main`. It already contains `cae960d3` (ElevenLabs model auto-selection; `resolve_preferred_elevenlabs_model` EXISTS in `crates/bookforge-audio/src/provider/elevenlabs.rs`) and `afcfe988` (OCR foundations). Build ON TOP.

**Environment (Windows, GNU toolchain).** Before ANY cargo command:
PowerShell — `$env:Path = "$env:USERPROFILE\.cargo\bin;$env:LOCALAPPDATA\mingw64\bin;$env:Path"`
bash — `export PATH="$USERPROFILE/.cargo/bin:$LOCALAPPDATA/mingw64/bin:$PATH"`
ffmpeg 8.0.1 + ffprobe are installed. python 3.14 available. poppler is NOT installed.

**Rules for every worker:**
- **Do NOT commit.** Leave changes uncommitted; the orchestrator reviews and commits.
- Touch only the files listed in your brief — other workers own the rest, in parallel.
- All new tests must be **offline, deterministic, Windows-friendly** (no `#[cfg(unix)]`, no poppler, no network). Existing idioms to reuse: `one_request_server` one-shot TCP mock (`bookforge-audio/src/provider.rs` test_support), `assert_cmd` CLI tests (`crates/bookforge-cli/tests/audiobook.rs`), the offline mock TTS provider.
- Finish with `cargo fmt --all` + your brief's test gate, then report: files changed, test counts, and any deviation from spec with the reason.

**Frozen contract (all workers code against these exact shapes):**
```rust
// bookforge-audio
pub enum NarrationBlockKind { Title, Heading(u8), Paragraph }
pub struct NarrationBlock { pub kind: NarrationBlockKind, pub text: String }
pub enum ChunkKind { Title, Heading, Body }          // serde: "title"|"heading"|"body"
pub struct NarrationChunk { pub kind: ChunkKind, pub text: String }

// SpeechRequest gains (all Option, provider-ignored where unsupported); add #[derive(Default)]
previous_text: Option<String>, next_text: Option<String>, seed: Option<u32>,
language_code: Option<String>, text_normalization: Option<TextNormalization>, // Auto|On|Off

// AudiobookOptions gains
context_chars: usize, seed: Option<u32>, language_code: Option<String>,
text_normalization: Option<TextNormalization>, heading_break_tag: Option<String>,
chapter_filter: Option<std::collections::BTreeSet<usize>>,

// StitchOptions gains
gap_chapter_ms: u32, gap_title_ms: u32, gap_paragraph_ms: u32,
loudnorm: bool, make_single: bool, author: Option<String>,

pub async fn list_elevenlabs_voices(base_url: &str, api_key: &str, timeout_seconds: u64)
    -> Result<Vec<ElevenLabsVoice>>;                  // ElevenLabsVoice{voice_id,name,category,labels}
pub async fn fetch_elevenlabs_subscription(config: &ElevenLabsTtsConfig)
    -> Result<ElevenLabsSubscription>;                 // {character_count, character_limit}
```

**Global schema changes (consistent across workers):**
- **Cache tag** in `synthesis_hash` (`builder.rs:570-586`): `"bookforge-audio-v1"` → `"bookforge-audio-v2"`, and fold in — length-prefixed, in this order — seed, language_code, text_normalization, previous_context, next_context, chunk-kind tag. Break tags live in chunk *text*, so they hash automatically. **All existing chunk caches invalidate**; CHANGELOG must say "rerun with `--prune`".
- **Manifest** `schema_version` 2 → 3. `ChunkRecord` gains `#[serde(default)] kind: ChunkKind`; `AudiobookManifest` gains `#[serde(default)]` `seed`, `language`, `text_normalization`, `gaps`, `author`. Serde-defaults keep v2 manifests readable by stitch/serve.
- **No** `OperationKind` unification (ROADMAP §10.1, owner decision 2026-07-15 stands).

---

## Phase 1 — W1 ∥ W3

### W1 — `bookforge-audio` core (the biggest; do WP-A → WP-B → WP-C-core → provider fns in order)
**Owns:** `crates/bookforge-audio/src/{text.rs, builder.rs, stitch.rs, provider.rs, provider/elevenlabs.rs, lib.rs}`. **Gate:** `cargo test -p bookforge-audio` green.

**WP-A narration structure**
- `text.rs`: add `NarrationBlockKind`/`NarrationBlock`. `Chapter.text: String` → `blocks: Vec<NarrationBlock>`, keeping `pub fn text(&self) -> String` (join `"\n\n"`) and `is_empty()` so consumers port trivially. In `chapters_from_book_with_options` read `block.kind` (the EPUB reader already yields `BlockKind::Heading(level)`); tag the section's **first** heading `Title` — it stays in the list exactly once, preserving today's "narrated once" semantics while `Chapter.title` is still derived as before. Mirror in `chapters_from_pdf_pages` (detected chapter-label line becomes that chapter's `Title`; merging only ever joins Paragraph↔Paragraph).
- `chunk_blocks(blocks, max_chars) -> Vec<NarrationChunk>`: Title/Heading blocks each become their **own** chunk (split only if one alone exceeds `max_chars`); Body reuses existing sentence-packing (`split_sentences`, `split_long_unit`); a chunk **never** spans a heading boundary. Keep `chunk_text` as a thin public wrapper.
- `builder.rs`: `build_plan` calls `chunk_blocks`; `PlannedChunk` + `ChunkRecord` gain `kind`. Filenames unchanged (`chapter-XXX-part-YYY-{hash16}.{ext}`; part numbering runs across all kinds).
- `stitch.rs` silence (**provider-independent, stream-copy preserving**): `StitchOptions` gains `gap_chapter_ms` (default 1200), `gap_title_ms` (800), `gap_paragraph_ms` (0). Add `probe_audio_params(path)` (ffprobe `stream=sample_rate,channels` of the first chunk) and `ensure_silence_file(out_dir, ms, ext, rate, channels)` → `ffmpeg -f lavfi -i anullsrc=r={rate}:cl={mono|stereo} -t {s} silence-{ms}.{ext}` with the ext's encoder (mp3→libmp3lame, wav→pcm_s16le, opus→libopus); cache by filename, clean up after. Insert entries into the existing concat lists: `gap_title_ms` after Title/Heading chunks, `gap_paragraph_ms` between body chunks when >0, `gap_chapter_ms` between chapters at book level. Add the inter-chapter gap to `assemble_m4b`'s running duration sum so markers stay correct. **ffprobe missing or probe fails → warn and stitch without gaps; never fail the run** (matches stitch's existing warning philosophy).
- Break tags: `AudiobookOptions.heading_break_tag: Option<String>`; when set, `build_plan` appends `" <break time=\"0.6s\" />"` to Title/Heading chunk text (so it hashes and counts in `chars`). Policy lives in the CLI (W4), not here.
- **Extract pure helpers for tests** so no ffmpeg is needed: `build_chapter_concat_entries(parts, gaps) -> Vec<String>` and the book-level equivalent.

**WP-B consistency**
- `provider.rs`: `SpeechRequest` gains the five context fields + `#[derive(Default)]` (keeps other providers' literals churn-free via `..Default::default()`). Mock/OpenAI/Gemini ignore them.
- `builder.rs`: `AudiobookOptions` gains `context_chars` (default 300; 0 disables), `seed`, `language_code`, `text_normalization`. In `build_audiobook`, **before** the spawn loop, precompute each chunk's `(previous_text, next_text)` from `plan[i±1]` **restricted to the same `chapter_index`**, taking the tail/head `context_chars` characters via a char-boundary-safe `context_slice` helper. Concurrency-safe: all derived from the ordered plan. Do **not** use `previous/next_request_ids` (would force serialization).
- `elevenlabs.rs`: `elevenlabs_request_body` emits `previous_text`, `next_text`, `seed`, `language_code`, `apply_text_normalization` when `Some`, omitted otherwise (mirror the existing `voice_settings`-at-default-speed omission).
- `synthesis_hash`: apply the global cache-tag change above. Add a code comment noting neighbor context means editing chunk N invalidates N±1 — intended.
- Loudness: `StitchOptions.loudnorm`; wire `-af loudnorm=I=-18:TP=-2:LRA=11` into the m4b re-encode (and the single-file re-encode below). Per-chapter files stay stream-copy — do **not** loudnorm chapters independently (per-chapter one-pass targets each chapter separately → level jumps).

**WP-C single-file core**
- `StitchOptions` gains `make_single`, `author`. New `assemble_single_file(...)` modeled on `assemble_m4b`: book-level concat list (with `gap_chapter_ms` silence) → `audiobook.{ext}`; **stream copy** when `!loudnorm` (silence is same-codec so copy stays valid), re-encode when loudnorm; add `-metadata title=... -metadata artist=...`. Reject `pcm`.
- `assemble_m4b`: add `-metadata title/artist` alongside the existing FFMETADATA chapter markers.
- `StitchReport` gains `single_file: Option<PathBuf>`.
- Extract `single_file_ffmpeg_args(...) -> Vec<String>` as a pure, testable fn.

**Provider fns (for W2/W4):** `fetch_elevenlabs_subscription` (GET `/v1/user/subscription`) and `list_elevenlabs_voices` (GET `/v1/voices`, takes an explicit `api_key` so the serve proxy can reuse it) in `elevenlabs.rs`, same `build_http_client`/`send_with_retry`/key-resolution shape as `resolve_preferred_elevenlabs_model`. Export via `provider.rs` + `lib.rs`.

**W1 tests:** heading typed correctly from a synthetic Book; title emitted exactly once; `chunk_blocks` never merges heading+body; ported `chunk_text` sentence-packing tests; plan's first chunk per chapter is `Title`; hash changes when any new knob changes + a v2-tag regression literal; `context_slice` stops at chapter boundaries and honors 0; concat-entry pure-fn tests with gaps; `build_ffmetadata` duration sums including gaps; `one_request_server` contract tests for the new ElevenLabs body fields (present when set, absent at defaults), `/v1/user/subscription`, `/v1/voices` (+ garbage-body error cases).

### W2 — WebUI parity, flag-independent half *(Phase 2 — run after W1 compiles)*
**Owns:** `crates/bookforge-cli/src/commands/serve.rs` + `crates/bookforge-cli/src/commands/serve/dashboard.{html,css,js}`. **Do NOT touch the byte-length/SHA literals yet** (W5 does that, once, last).

- **Auto model:** `AudioProviderOption` gains `supports_auto_model` (true only for elevenlabs). In `launch_audiobook`, an empty/absent `model` for elevenlabs means **omit `--model`** from the spawned child → the CLI's auto-selection engages. `audio_provider_max_chars` for the auto case validates against `10_000` (the resolver guarantees a model whose limit ≥ max_chars; error text should say "pick eleven_flash_v2_5 explicitly for >10k"). In `dashboard.js renderAudiobook`, render the elevenlabs model field as a `<select id="a_model">` whose first option is `Auto (recommended)` with value `""` (other providers keep free-text + datalist); `bfLaunchAudiobook` appends `model` only when non-empty; `audioProviderMaxChars` returns 10000 for `""`.
- **Resolved model:** `audiobook_status` already returns the manifest's `synthesis_id` (`elevenlabs:{base_url}:{model}` — base_url contains colons, so split on the **last** `:`); add a `resolved_model` field and show `Model: X (auto-selected)` in `pollAudiobook`.
- **Voice picker proxy:** new route `GET /api/audio/voices?provider=elevenlabs` → handler resolves the key exactly like `launch_audiobook` does (session key slot `audio:elevenlabs`, else the `ELEVENLABS_API_KEY` env detection), calls `bookforge_audio::list_elevenlabs_voices`, caches in `AppState` behind a 5-minute TTL. **Never** put the key in the response, logs, or a URL; return **409** (not 401-with-detail) when no key is stored. GET needs no CSRF (matches existing conventions). dashboard.js renders a `<select>` of `name — voice_id` when the fetch succeeds, silently falling back to the current free-text input otherwise.
- **Playback:** support `?disposition=inline` on `audiobook_artifact` (`Content-Disposition: inline`); on the finished panel render `<audio controls preload="none" src=".../artifact?disposition=inline">` next to the existing Download button.
- **Progress screen:** aggregate `chunks` client-side by `chapter_index`/`chapter_title` into a per-chapter done/total list (scrollable, check badge when complete); show the resolved model.
- **Tests:** `audio_voices` with no key → 409; resolved-model split-on-last-colon unit test; refactor the child-process argv construction inside `launch_audiobook` into a testable `fn audiobook_command_args(...) -> Vec<OsString>` and assert the auto case omits `--model`. Keep `dashboard_ships_all_screen_renderers`, the no-`window.prompt` assertion, and the CSRF-header assertion green (every new mutating fetch must send `headers:{[CSRF_HEADER]:CSRF_TOKEN}`).

### W3 — docs skeleton *(Phase 1 — parallel with W1; touches no code)*
**Owns:** `docs/ROADMAP.md`, `docs/audiobooks.md`, `CHANGELOG.md`.
- ROADMAP: append `## 16. v3.0 — Audiobook narration quality & UI parity` immediately before `*End of document.*` (:2925), following the milestone template at :38-49 (Goal / Architectural rationale / Deliverables / Schema changes / CLI surface / Implementation notes / Out of scope / Acceptance criteria / Effort / Dependencies). Schema-changes section must list manifest v3, cache tag v2, and the new audio pricing file. Out of scope: per-chapter streaming route, `request_ids` serialization, OperationKind unification (§10.1), Range support on the artifact route. Acceptance criteria include one live end-to-end. Add a `v3.0` row to the §2 overview table (:113-125) with status `in progress`; bump the doc header to version 1.4.0 / 2026-07-20.
- `docs/audiobooks.md`: restructure for the full new flag surface (leave `TODO(worker)` markers for flags W4 finalizes); document gap semantics, the chaptered-by-default deliverable, flat-mp3 opt-in, `--list-voices`, quota/cost lines.
- `CHANGELOG.md` `## Unreleased`: bullets for every WP, with a prominent **"existing audio chunk caches are invalidated (hash v2); rerun with `--prune`"** notice.

## Phase 2 — W2 ∥ W4 (both require W1)

### W4 — CLI wiring
**Owns:** `crates/bookforge-cli/src/commands/audiobook.rs`, new `crates/bookforge-cli/src/audio_cost.rs`, new `crates/bookforge-cli/pricing/audio-providers.json`, `main.rs` (module registration), `crates/bookforge-cli/src/tui/mod.rs`, `crates/bookforge-cli/tests/audiobook.rs`.
- **Flags:** `--gap-chapter-ms` (1200) / `--gap-title-ms` (800) / `--gap-paragraph-ms` (0); `--break-tags auto|off` (default auto → set the tag **only** for elevenlabs flash/turbo/multilingual v2; **never** eleven_v3, never other providers); `--seed <u32>` (error on non-elevenlabs); `--language <code>` (default auto from `book.metadata.language`, normalized to the primary subtag, e.g. `en-US`→`en`; applied **only** for flash/turbo v2.5 — on multilingual_v2/v3 warn and drop, since the API rejects it); `--text-normalization auto|on|off`; `--loudnorm`; `--single`; `--chapters <RANGE>`; `--list-voices`.
- **Chaptered-by-default:** when ffmpeg is available a normal run now emits the chaptered `.m4b` (with title/author from `book.metadata`) without an explicit `--m4b`; add `--no-book-file` to opt out. Keep `--stitch`/`--m4b` working as explicit overrides. `--single` adds the flat file and is combinable.
- `--chapters`: `parse_chapter_ranges(&str) -> Result<BTreeSet<usize>>` (1-based, matching printed chapter numbers; reject `0`, reversed ranges, garbage). Filter inside `build_plan` via `AudiobookOptions.chapter_filter` so plan/dry-run/manifest/serve all stay consistent; keep global chapter numbering in filenames.
- `--list-voices`: make `input` optional and bail "INPUT is required" unless `--list-voices`; elevenlabs-only; print a `voice_id  name  labels` table. Improve the missing-`--voice` error to point at it.
- **Cost + quota:** new `pricing/audio-providers.json` (`schema_version: 1`, per provider/model `{usd_per_million_chars?, credits_per_char?}`, at least one required; seed ElevenLabs from the observed **~0.27 credits/char** for v3 on payg plus published relative multipliers, and set `updated_at`). `audio_cost.rs` mirrors `cost.rs` (`include_str!`, `BOOKFORGE_AUDIO_PRICING_PATH` override, hard `schema_version == 1` check) exposing `estimate_audio_cost(provider, model, chars)`. Print `Estimated cost: ~$X.XX / ~N credits` after the existing `Plan:` line and add the fields to the `audiobook_plan` JSON event. On elevenlabs runs (live always; dry-run when the key env is set) call `fetch_elevenlabs_subscription` and print `ElevenLabs quota: N remaining of M`, warning when the plan exceeds it — **preflight failure is a warning, never fatal** (mirror the existing auto-selection fallback style).
- **TUI:** `AudioTuiInfo` gains a preformatted `cost_line: Option<String>` and `chapters_total`; render resolved model + cost; per-chapter progress if trivial.
- **Tests:** `parse_chapter_ranges` unit tests; mock-provider `assert_cmd` runs asserting `--chapters 2` produces only chapter-002 records, that a `<h1>`+2`<p>` fixture yields the title alone in part 1 (inspect `manifest.json`), and both `--list-voices` misuse directions; `audio_cost` parse/reject-schema/lookup tests + a `BOOKFORGE_AUDIO_PRICING_PATH` temp-file test asserting the printed cost line; language-normalization unit test.

## Phase 3 — W5 (after W2 and W4)

### W5 — serve knob wiring + the one byte-literal update
**Owns:** `serve.rs` + `dashboard.{html,css,js}` (same files as W2 — run after it).
- **Estimate endpoint:** `POST /api/audiobook/estimate` (CSRF-checked, multipart `file`/`provider`/`model`/`max_chars`/`chapters?`): temp-save the upload, `spawn_blocking` → `read_epub` + `plan_chunks`, delete the temp, respond `{chapters, chunks, characters, est_cost_usd, est_credits}` via `audio_cost`; when provider is elevenlabs and a key is available, add `quota: {remaining, limit, fits}` from `fetch_elevenlabs_subscription`. dashboard.js `bfAudioEstimate()` auto-fires debounced on file/provider/model/max-chars change (mirror the translate wizard's `requestEstimate`), rendering a cost + quota line above the launch button.
- **Advanced knobs** in a `<details>` section (main form stays as-is): Chapter pause select none/short/medium/long → `gap_chapter_ms` 0/600/1200/2000 (title gap derived as `min(800, chapter_gap)`, no separate control); **Flat mp3 for on-the-go** → `--single`; **Normalize loudness** → `--loudnorm`; **Seed** (elevenlabs); **Language** (placeholder "auto (from EPUB)", flash/turbo only). Server parses and forwards with validation (`gap_chapter_ms` clamp 0..=10_000, `seed` u32, `language` `[a-zA-Z-]{2,8}`).
- Verify the server/JS max-chars tables stay in sync (extend the existing test).
- **Last step, once:** run `cargo test -p bookforge-cli --features serve dashboard_assets_reassemble_byte_stably`, take the actual byte length + SHA-256 from the failure output, update both literals in `serve.rs` (~:2924 and ~:2931). Add markers (`function bfAudioEstimate`, `Auto (recommended)`, `/api/audio/voices`) to the renderer/marker tests.

## Phase 4 — orchestrator
Full `cargo test --workspace`; docs finalized (W3 `TODO(worker)` markers resolved against W4's real flags); release prep commit bumping all 7 crate versions to `3.0.0` + dated `## v3.0.0` CHANGELOG heading, mirroring commit `bd1faf11` (`git show bd1faf11` is the checklist). Tagging `v3.0.0` triggers cargo-dist `release.yml`.

## Verification
- `cargo test --workspace` green after each phase; all new tests offline/deterministic/Windows-friendly.
- **Live ElevenLabs checkpoints (orchestrator-run, small):** after **WP-A** — title/heading separation audible; after **WP-B** — consistency + seed reproducibility; after **WP-C** — flat mp3 + m4b metadata + dashboard playback. Each uses `tests/Che_fare_Lenin.it.epub` narrating only *Correzione* (~2k chars) via `--chapters`, on `eleven_flash_v2_5`. Track spend; hard stop at ≤90% of remaining credits.
- **Milestone acceptance:** a full dashboard run — auto-model → voice picker → estimate/quota → launch → per-chapter progress → in-page playback → download.

## Risks
Break-tag support varies by model (auto set includes multilingual_v2 — live-smoke it; if tags get read aloud, drop it from the set: one line, hash-safe). Stream-copy concat with generated silence needs ffprobe-matched rate/channels; opus-in-ogg is the shakiest → warn-and-skip-gaps fallback, never a failed run. Loudnorm only at book assembly means per-chapter downloads are un-normalized (documented). Voice/quota proxy: key stays server-side, 409 not 401. Dashboard byte-test churn: only W5 updates the literals. Hash-context coupling: editing chunk N invalidates N±1 (documented). `<audio>` seeking on a long m4b is limited without Range support — ship inline playback now, Range as a §16 follow-up.
