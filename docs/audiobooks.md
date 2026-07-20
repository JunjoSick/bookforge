# Audiobook Generation

`bookforge audiobook` turns an EPUB into narrated audio. It works directly on
a source EPUB as well as on a translated EPUB; translation is optional. It reuses the same
design principle as the translation engine: deterministic Rust owns the
structure — chapter extraction, chunking, file layout, and resume — and the
speech provider only ever receives a plain text chunk and returns audio bytes.

The workflow is available consistently in three places: the ordinary CLI,
the full-screen terminal dashboard (`--ui tui`), and the Audiobooks screen in
the local browser dashboard started by `bookforge serve`.

## Pipeline

1. **Read** the EPUB into the internal representation.
2. **Extract chapters** from the spine documents. The synthetic sections the
   reader builds for translation (OPF metadata, NCX table of contents) are
   skipped — you do not want a table of contents read aloud. Inline markers
   are stripped so the narrator hears clean prose.
3. **Chunk** each chapter on sentence boundaries into pieces under
   `--max-chars` (default 2000; the maximum is provider-specific). Chunking is
   a pure function of the text, so a resumed run re-derives the exact same
   chunks.
4. **Synthesize** each chunk through the TTS provider, `--concurrency` at a
   time, writing `chapter-NNN-part-NNN-<hash>.<ext>` atomically (temp file then
   rename) so an interrupted write is never mistaken for a finished one. The
   hash covers the text, provider/model identity, voice, format, speed, and
   instructions.
5. **Manifest**: a `manifest.json` records the plan and per-chunk metadata.
6. **Stitch** (optional): with `--stitch` or `--m4b`, ffmpeg joins the parts.

## Resume

Re-run the exact same command and every matching hashed chunk already on disk
is skipped. Changing the source text, model, endpoint, voice, format, speed, or
instructions produces a new hash and therefore cannot silently reuse stale
audio. This makes long books safe against network hiccups, rate-limit
interruptions, and Ctrl-C while keeping old generations auditable. The run
summary reports how many chunks were synthesized versus reused.

Because a changed voice, model, speed, format, or edited source produces new
file names, the superseded chunks stay on disk. Nothing deletes them
automatically. Pass `--prune` to remove the chunk files the *current* run does
not use and free the space; combine it with `--dry-run` to list what would be
removed without deleting anything. Stitched per-chapter outputs, the assembled
`.m4b`, and `manifest.json` are never touched.

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

- **`gemini`** - Google's native Gemini Generate Content TTS API. The default
  model is `gemini-3.1-flash-tts-preview`, the default voice is `Kore`, and the
  key defaults to `GEMINI_API_KEY`. Gemini returns 24 kHz mono PCM; BookForge
  wraps it in a real WAV by default so it can be played and stitched safely.
  Raw `--format pcm` is also available.

  ```bash
  bookforge audiobook book.epub --provider gemini \
    --voice Kore \
    --instructions "Read as a calm, precise audiobook narrator." \
    --format wav --m4b
  ```

- **`elevenlabs`** - ElevenLabs' native text-to-speech API, authenticated with
  `ELEVENLABS_API_KEY` by default. Pass an ElevenLabs voice ID with `--voice`;
  voice names are not interchangeable with IDs. When `--model` is omitted on
  a live run, BookForge checks the account's available TTS models and selects
  the first compatible model in this order: `eleven_v3`,
  `eleven_flash_v2_5`, `eleven_turbo_v2_5`, then
  `eleven_multilingual_v2`. An explicit `--model` bypasses auto-selection. A
  dry run stays offline and reports the static `eleven_multilingual_v2`
  default instead. Supported outputs are MP3, Opus, WAV, and PCM.

  ```bash
  bookforge audiobook book.epub --provider elevenlabs \
    --voice JBFqnCBsd6RMkjVDRZzb \
    --model eleven_multilingual_v2 --format mp3 --m4b
  ```

  ElevenLabs does not expose a free-form instructions field on this endpoint.
  Configure the selected voice in ElevenLabs, use `--speed`, or place
  model-supported audio tags directly in the source text. `eleven_v3` does not
  support speed control on the TTS endpoint, so it requires `--speed 1.0`.
  BookForge enforces the selected model's per-request character limit.

- **`mock`** — a deterministic, offline provider that emits valid silent WAV
  clips scaled to the text length. Its format is always `wav`; BookForge uses
  that default automatically and rejects an explicitly mismatched format.

## Options

| Flag | Default | Purpose |
| --- | --- | --- |
| `--out <dir>` | `<input>.audiobook` | Output directory. |
| `--provider <mock\|openai\|gemini\|elevenlabs>` | `openai` | Speech backend. |
| `--model <name>` | provider-specific | TTS model. |
| `--voice <name-or-id>` | provider-specific | Voice name, or ElevenLabs voice ID. |
| `--format <mp3\|opus\|aac\|flac\|wav\|pcm>` | provider-specific | Output codec/container. |
| `--speed <f32>` | `1.0` | Playback speed multiplier. |
| `--base-url <url>` | OpenAI | Endpoint override for local servers. |
| `--api-key-env <VAR>` | provider-specific | Env var holding the key. |
| `--max-chars <n>` | `2000` | Max characters per request. |
| `--concurrency <n>` | `4` | Parallel synthesis requests. |
| `--timeout-seconds <n>` | `120` | Per-request timeout. |
| `--instructions <text>` | none | Delivery or pronunciation guidance for models that support it. |
| `--stitch` | off | Join each chapter's parts into one file (ffmpeg). |
| `--m4b` | off | Also assemble a single `.m4b` with chapter markers. |
| `--dry-run` | off | Print the chapter/chunk plan and exit. |
| `--prune` | off | Delete chunk files from earlier runs the current plan does not use (report-only with `--dry-run`). |
| `--ui <auto\|progress\|quiet\|json\|tui>` | `auto` | Select progress output or the full-screen terminal dashboard. |

## Stitching

`--stitch` and `--m4b` shell out to `ffmpeg`. If it is not on `PATH`, plain
`--stitch` keeps the per-chunk files and reports a warning. Explicit `--m4b`
returns an error when the requested single-book artifact cannot be built; the
completed chunk files remain available for a resumed stitch.

- `--stitch` concatenates each chapter's parts into
  `chapter-NNN-<title>.<ext>` with a stream copy (no re-encode).
- `--m4b` additionally assembles a single `audiobook.m4b`. When `ffprobe` is
  available it measures each chapter's duration and writes chapter markers so
  players can jump between chapters; without `ffprobe` you still get a
  playable `.m4b`, just without the marker list.

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
- `--stitch`/`--m4b` reject raw PCM because it has no container-level sample
  metadata. Use WAV when you need uncompressed intermediate files.
- Cost and time scale with characters, not chunks. Use `--dry-run` to see the
  character total before committing to a paid run.

## Narrating a Toki Pona translation

If a Toki Pona edition is desired, generate the translated EPUB first and then
narrate that output. Current OpenAI
speech models accept delivery instructions, which is useful for consistent
Toki Pona pronunciation:

```bash
bookforge audiobook lipu.tok.epub \
  --model gpt-4o-mini-tts \
  --voice alloy \
  --instructions "Speak Toki Pona clearly and evenly; pronounce every vowel consistently." \
  --m4b
```
