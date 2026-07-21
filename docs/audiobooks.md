# Audiobook Generation

`bookforge audiobook` turns an EPUB into narrated audio. It works directly on
a source EPUB as well as on a translated EPUB; translation is optional. It
reuses the same design principle as the translation engine: deterministic Rust
owns structure — chapter extraction, narration hierarchy, chunking, file
layout, and resume — and the speech provider only ever receives text and
returns audio bytes.

The workflow is available consistently in three places: the ordinary CLI, the
full-screen terminal dashboard (`--ui tui`), and the Audiobooks screen in the
local browser dashboard started by `bookforge serve`.

> **Upgrading to v2.6.0 invalidates all existing audio chunk caches.** The cache
> hash moved to `bookforge-audio-v2` and the manifest to schema 3. Rerun the
> audiobook command with `--prune` to remove superseded v2.5 chunk files after
> reviewing the new plan; use `--prune --dry-run` to preview what will be
> deleted.

## Pipeline

1. **Read** the EPUB into the internal representation.
2. **Extract chapters** from the spine documents. The synthetic sections the
   reader builds for translation (OPF metadata, NCX table of contents) are
   skipped — you do not want a table of contents read aloud. Inline markers
   are stripped so the narrator hears clean prose.
3. **Preserve narration hierarchy.** A chapter title and every in-section
   heading become their own typed narration blocks. Each title or heading gets
   a separate chunk and is never packed into the following body prose. The
   title remains present exactly once.
4. **Chunk body prose** on sentence boundaries into pieces under
   `--max-chars` (default 2000; the maximum is provider-specific). Chunking is
   a pure function of the text, so a resumed run re-derives the same plan.
5. **Synthesize** each chunk through the TTS provider, `--concurrency` at a
   time, writing `chapter-NNN-part-NNN-<hash>.<ext>` atomically (temp file then
   rename) so an interrupted write is never mistaken for a finished one.
6. **Manifest**: a schema-3 `manifest.json` records the plan, narration kind,
   synthesis settings, gap settings, author, and per-chunk metadata.
7. **Assemble**: when ffmpeg is available, a normal run creates a chaptered
   `audiobook.m4b` with chapter markers and title/artist metadata (using the
   EPUB author as the artist). The chunks remain the resumable units on disk.

## Narration hierarchy and continuity

The default pauses are intentionally audiobook-like without making ordinary
prose sound halting:

- 1200 ms between chapters gives a clear scene break.
- 800 ms after a chapter title or in-section heading lets the heading stand
  apart from the prose it introduces.
- 0 ms between body chunks avoids adding an audible pause at boundaries that
  exist only because of a provider request limit.

These gaps are inserted during stitching, not synthesized into every audio
chunk. BookForge probes the first chunk's sample rate and channel count so the
generated silence can be concatenated without re-encoding. If ffprobe is
missing or cannot inspect the audio, BookForge warns and assembles without
gaps rather than failing the run.

For ElevenLabs, BookForge also sends up to 300 characters of `previous_text`
and `next_text` around each request. Context never crosses a chapter boundary.
This improves delivery across chunk joins while preserving parallel synthesis;
editing one chunk can therefore invalidate its immediate neighbors as well.
The optional seed, language, and text-normalization controls are included in
the cache key.

With `--break-tags auto`, BookForge appends an ElevenLabs
`<break time="0.6s" />` tag to title and heading text for Flash v2.5, Turbo
v2.5, and Multilingual v2. It never adds these tags for Eleven v3 or another
provider. Use `--break-tags off` when the selected voice or model reads tags
aloud instead of treating them as directives.

## Providers

- **`openai`** (default) — any endpoint speaking the OpenAI `/audio/speech`
  API. Works with OpenAI itself and with local servers such as
  [kokoro-fastapi](https://github.com/remsky/Kokoro-FastAPI) or
  openai-edge-tts. Point at a local server with `--base-url`:

  ```bash
  bookforge audiobook book.epub \
    --base-url http://localhost:8880/v1 \
    --api-key-env KOKORO_API_KEY \
    --model kokoro --voice af_sky --format mp3
  ```

  The key is read from the environment variable named by `--api-key-env`
  (default `OPENAI_API_KEY`) and is never placed on the command line. Loopback
  endpoints (`localhost`, `127.0.0.1`, or `::1`) do not require a key.

- **`gemini`** — Google's native Gemini Generate Content TTS API. The default
  model is `gemini-3.1-flash-tts-preview`, the default voice is `Kore`, and the
  key defaults to `GEMINI_API_KEY`. Gemini returns 24 kHz mono PCM; BookForge
  wraps it in a real WAV by default so it can be played and stitched safely.
  Raw `--format pcm` is also available.

  ```bash
  bookforge audiobook book.epub --provider gemini \
    --voice Kore \
    --instructions "Read as a calm, precise audiobook narrator." \
    --format wav
  ```

- **`elevenlabs`** — ElevenLabs' native text-to-speech API, authenticated with
  `ELEVENLABS_API_KEY` by default. Pass an ElevenLabs voice ID with `--voice`;
  voice names are not interchangeable with IDs. Run the following first to
  discover the account's available voices:

  ```bash
  bookforge audiobook --provider elevenlabs --list-voices
  ```

  When `--model` is omitted on a live run, BookForge checks the account's
  available TTS models and selects the first compatible model in this order:
  `eleven_v3`, `eleven_flash_v2_5`, `eleven_turbo_v2_5`, then
  `eleven_multilingual_v2`. An explicit `--model` bypasses auto-selection.
  Supported outputs are MP3, Opus, WAV, and PCM.

  ```bash
  bookforge audiobook book.epub --provider elevenlabs \
    --voice JBFqnCBsd6RMkjVDRZzb \
    --model eleven_flash_v2_5 --format mp3
  ```

  ElevenLabs does not expose a free-form instructions field on this endpoint.
  Configure the selected voice in ElevenLabs, use `--speed`, or enable the
  heading break-tag policy. `eleven_v3` does not support speed control on the
  TTS endpoint, so it requires `--speed 1.0`. BookForge enforces the selected
  model's per-request character limit.

- **`mock`** — a deterministic, offline provider that emits valid silent WAV
  clips scaled to the text length. Its format is always `wav`; BookForge uses
  that default automatically and rejects an explicitly mismatched format.

## Options

### General and planning

| Flag | Default | Purpose |
| --- | --- | --- |
| `--out <dir>` | `<input>.audiobook` | Output directory. |
| `--provider <mock\|openai\|gemini\|elevenlabs>` | `openai` | Speech backend. |
| `--model <name>` | provider-specific; auto for ElevenLabs | TTS model. |
| `--voice <name-or-id>` | provider-specific | Voice name, or ElevenLabs voice ID. |
| `--format <mp3\|opus\|aac\|flac\|wav\|pcm>` | provider-specific | Output codec/container. |
| `--speed <f32>` | `1.0` | Playback speed multiplier. |
| `--base-url <url>` | OpenAI | Endpoint override for local servers. |
| `--api-key-env <VAR>` | provider-specific | Environment variable holding the key. |
| `--max-chars <n>` | `2000` | Maximum characters per request. |
| `--concurrency <n>` | `4` | Parallel synthesis requests. |
| `--timeout-seconds <n>` | `120` | Per-request timeout. |
| `--instructions <text>` | none | Delivery or pronunciation guidance for models that support it. |
| `--chapters <RANGE>` | all chapters | Narrate one-based chapter numbers and ranges. |
| `--list-voices` | off | List ElevenLabs voice IDs and metadata; no input EPUB is required. |
| `--dry-run` | off | Print the chapter/chunk plan, cost estimate, and available quota, then exit. |
| `--prune` | off | Delete superseded chunks not used by the current plan (report-only with `--dry-run`). |
| `--ui <auto\|progress\|quiet\|json\|tui>` | `auto` | Select progress output or the full-screen terminal dashboard. |

Chapter numbers are the one-based numbers shown in the plan. Comma-separated
items and inclusive ranges can be mixed:

```bash
bookforge audiobook book.epub --chapters 2,5-7 --dry-run
```

Zero, reversed ranges, and invalid text are rejected. Selected chapters keep
their original numbers in the manifest and output filenames.

### Structure, consistency, and output

| Flag | Default | Purpose |
| --- | --- | --- |
| `--gap-chapter-ms <MS>` | `1200` | Silence between chapters. |
| `--gap-title-ms <MS>` | `800` | Silence after titles and headings. |
| `--gap-paragraph-ms <MS>` | `0` | Silence between body chunks. |
| `--break-tags <auto\|off>` | `auto` | Add heading break tags on compatible ElevenLabs v2 models. |
| `--seed <u32>` | none | Request reproducible ElevenLabs generation; rejected for other providers. |
| `--language <code>` | primary subtag from EPUB metadata | Send a language code to ElevenLabs Flash/Turbo v2.5. |
| `--text-normalization <auto\|on\|off>` | `auto` | Control ElevenLabs text normalization. |
| `--loudnorm` | off | Normalize the assembled book to `I=-18:TP=-2:LRA=11`. |
| `--no-book-file` | off | Do not create the default chaptered `.m4b`. |
| `--single` | off | Also create a flat `audiobook.<format>` for on-the-go listening. |
| `--stitch` | off | Explicitly request stitched per-chapter files. |
| `--m4b` | automatic when ffmpeg is present | Explicitly request the chaptered `.m4b`. |

`--language` normalizes metadata such as `en-US` to its primary subtag (`en`).
It is sent only to ElevenLabs Flash and Turbo v2.5. BookForge warns and drops
it for every other model or provider.
The `--text-normalization` setting maps to ElevenLabs'
`apply_text_normalization` request field.

## Cost and quota preflight

After the `Plan:` summary, BookForge estimates the narration price from the
bundled schema-1 `crates/bookforge-cli/pricing/audio-providers.json` table and
prints the applicable dollar and/or provider-credit estimate, for example:

```text
Estimated cost: ~$1.23 / ~4500 credits
```

Pricing is per character, so changing chunk size does not change the estimate.
These figures are planning estimates, not invoices: provider plans and rates
can change, and the provider's own billing is authoritative.

Before a live ElevenLabs run, BookForge queries `/v1/user/subscription` and
prints a line such as `ElevenLabs quota: 12000 remaining of 20000`. It warns
when the plan exceeds the remaining quota. A dry run performs the same check
when the configured key environment variable is available. Network or quota
preflight failure is always a warning and never prevents synthesis.

## Book files, stitching, and loudness

When ffmpeg is on `PATH`, the default deliverable is `audiobook.m4b`, with
chapter markers plus title/artist metadata drawn from the EPUB's title and
author. `--no-book-file`
opts out. `--single` adds one flat `audiobook.<format>`; with MP3 output this is
the convenient single-file MP3 for players that do not need chapter markers.
The two book outputs can be requested together.

`--stitch` and `--m4b` remain available as explicit overrides. Per-chapter
stitching and ordinary flat-file assembly use stream copy. Raw PCM is rejected
for stitching because it has no container-level sample metadata; choose WAV
for uncompressed intermediates.

`--loudnorm` applies ffmpeg's loudness filter once while assembling the `.m4b`
and flat file. It deliberately does not normalize each chapter independently,
which would allow target differences to create audible level jumps. Individual
chapter downloads therefore remain unnormalized.

If ffmpeg is unavailable, BookForge keeps the completed chunks and warns that
it could not create the automatic book file. An explicitly requested `--m4b`
fails clearly while preserving those chunks for a later resumed stitch.

## Resume and cache cleanup

Re-run the same command and every matching hashed chunk already on disk is
skipped. The `bookforge-audio-v2` cache key covers the text, narration kind,
provider/model identity, voice, format, speed, instructions, seed, language,
normalization, and same-chapter neighbor context. An interrupted write is
never accepted as a finished chunk. The run summary reports how many chunks
were synthesized and reused.

Superseded chunks stay on disk so prior generations remain auditable. Pass
`--prune` to remove chunk files the current run does not use and free the
space; combine it with `--dry-run` to list them without deleting anything.
Stitched per-chapter outputs, assembled book files, and `manifest.json` are
never pruned.

## Browser dashboard

The Audiobooks screen mirrors the CLI's current workflow:

- ElevenLabs offers `Auto (recommended)` model selection and retrieves the
  account's voices through a server-side proxy; the API key never reaches the
  browser. If voice retrieval fails, the form falls back to a voice-ID field.
- A pre-launch estimate reports chapters, chunks, characters, cost, and
  ElevenLabs quota before synthesis starts.
- Running jobs show per-chapter completed/total progress and the resolved
  auto-selected model.
- The Advanced section exposes chapter pause, flat-file output, loudness
  normalization, seed, and language. Chapter-pause presets map none/short/
  medium/long to 0/600/1200/2000 ms; the title gap is derived as the smaller
  of 800 ms and the selected chapter gap.
- The finished panel plays the chaptered `.m4b` in-page and keeps the download
  action available.

The artifact route does not yet support HTTP Range requests, so seeking through
a long `.m4b` may depend on browser buffering behavior.

## Notes

- Hosted OpenAI speech requests are capped at 4096 input characters; BookForge
  rejects a larger `--max-chars` value for that endpoint.
- ElevenLabs limits are model-specific: 40,000 input characters for Flash and
  Turbo v2.5, 10,000 for Multilingual v2, and 5,000 for Eleven v3. The CLI and
  dashboard reject larger values, and the provider checks the final request as
  a last line of defense.
- Gemini TTS supports WAV and raw PCM in BookForge. ElevenLabs supports MP3,
  Opus, WAV, and raw PCM; some high-sample-rate formats require a qualifying
  ElevenLabs plan.
- Cost and time scale with characters, not chunks. Use `--dry-run` and
  `--chapters` to inspect a small subset before committing to a paid run.

## Narrating a Toki Pona translation

If a Toki Pona edition is desired, generate the translated EPUB first and then
narrate that output. Current OpenAI speech models accept delivery instructions,
which is useful for consistent Toki Pona pronunciation:

```bash
bookforge audiobook lipu.tok.epub \
  --model gpt-4o-mini-tts \
  --voice alloy \
  --instructions "Speak Toki Pona clearly and evenly; pronounce every vowel consistently."
```
