# Troubleshooting BookForge

Start with these three checks:

```bash
bookforge --version
bookforge doctor --storage
bookforge status <job-id>
```

Then use the section that matches the failure. Error reports should include the
BookForge version, operating system, provider and model, the command with all
secrets removed, and a redacted validation or QA report.

## `bookforge` is not found

Close and reopen the terminal after using a prebuilt installer so its `PATH`
refreshes. Confirm the binary from a new terminal:

```bash
bookforge --version
```

When running from a source checkout, use:

```bash
cargo run -p bookforge-cli -- --version
```

Building from source requires Rust 1.88 or newer. The prebuilt installers do
not require Rust, Cargo, Git, Python, or Node.

## The dashboard does not open

Start it explicitly and open the printed address yourself:

```bash
bookforge serve --open
```

The default address is `http://127.0.0.1:8765`. BookForge intentionally
accepts only a loopback bind because the dashboard is unauthenticated and can
launch runs with session-only provider keys. Do not expose it through a public
interface or reverse proxy.

If port 8765 is occupied, choose another loopback port:

```bash
bookforge serve --bind 127.0.0.1:8877 --open
```

## A job is missing

BookForge opens `.bookforge/jobs.sqlite` relative to the current working
directory. Return to the directory where the job was created and retry:

```bash
bookforge status <job-id>
```

The dashboard follows the same rule. Starting it from another folder displays
that folder's independent job library.

Do not move only `jobs.sqlite`: run directories contain event logs, snapshots,
review artifacts, and control files referenced by the database. Move or back
up the entire `.bookforge/` directory together.

## The API key is missing or rejected

The browser dashboard accepts a key for the life of that server process and
does not write it to disk. CLI commands read keys from environment variables.

Bash or zsh:

```bash
export DEEPSEEK_API_KEY="..."
export OPENROUTER_API_KEY="..."
export OPENAI_API_KEY="..."
```

PowerShell:

```powershell
$env:DEEPSEEK_API_KEY = "..."
$env:OPENROUTER_API_KEY = "..."
$env:OPENAI_API_KEY = "..."
```

Check the exact provider and model before translating:

```bash
bookforge doctor --provider openrouter --model google/gemini-2.5-flash-lite
```

For a custom OpenAI-compatible endpoint, make the key variable explicit with
`--api-key-env`, and check that `--base-url` includes the provider's `/v1` base
when required. See [PROVIDERS.md](PROVIDERS.md).

## A translation appears stuck

First inspect persisted state rather than restarting blindly:

```bash
bookforge status <job-id>
bookforge tail <job-id> --last 60
bookforge watch <job-id>
```

An in-flight provider request can take up to the configured timeout. Rate-limit
backoff and response validation retries also make a segment take longer without
losing earlier checkpoints.

If the worker is no longer running, `resume` starts a replacement and reuses
completed work:

```bash
bookforge resume <job-id>
```

If the job is paused and its original worker is still alive, normal `resume`
wakes it. Use `resume --force` only when you know that paused process is gone;
two live workers can duplicate provider requests.

## Pause or stop is not immediate

Both controls are cooperative. BookForge finishes or safely abandons the
current request, checkpoint, or finalize-stage boundary before changing state.
This protects the EPUB and job database from partial writes.

Use `status` or `tail` to confirm the resulting state:

```bash
bookforge pause <job-id>
bookforge status <job-id>
```

## Reconfiguration is rejected

Only settings that preserve checkpoint and cache compatibility can change on
an existing job. Concurrency, batch budgets, provider attempts, adaptive
sizing, QA, double-check, and output validation are mutable.

Provider, model, languages, prompt/profile, segmentation, context, glossary,
style, and entity guidance are immutable for that job. Start a new translation
when one of those needs to change. Also check that the job is running, paused,
or stopped and still has remaining work.

## Segments are `failed` or `needs_review`

`failed` usually means provider attempts were exhausted. `needs_review` means
BookForge preserved safety by refusing to accept suspect output, or that a
human flag requested review.

Generate the side-by-side report before retrying everything:

```bash
bookforge review <job-id> --open
```

Then choose a scope and resume:

```bash
bookforge retry <job-id> --only failed
bookforge retry <job-id> --only needs-review
bookforge resume <job-id>
```

`retry` only marks segments `retry_pending`; `resume` performs the requests.
For a known correct translation, use `bookforge correct` instead of spending
another provider request. See [CLI_REFERENCE.md](CLI_REFERENCE.md#review-flag-and-correct).

## Validation cannot find EPUBCheck

BookForge's built-in structural validation still runs. Missing EPUBCheck is
reported as `status: unavailable` and is non-fatal unless your own workflow
requires it.

Set `BOOKFORGE_EPUBCHECK` to one of:

- the EPUBCheck executable;
- the directory containing the executable; or
- an `epubcheck.jar` file.

Run validation again and inspect the JSON report:

```bash
bookforge validate output.epub --report output.validation.json
```

`--strict-epubcheck` turns EPUBCheck warnings into validation failures; omit it
when warnings are acceptable.

## EPUB inspection reports low text coverage

Do not start a paid translation until you understand the missing coverage.
`inspect` lists files whose visible text sits outside supported translatable
blocks. That text would otherwise remain in the source language.

If the EPUB came from a PDF or contains broken paragraph boundaries, preview an
explicit reflow:

```bash
bookforge reflow source.epub --output source.reflowed.epub --dry-run
```

Review the report before running without `--dry-run`. Reflow repairs flow; it
does not make arbitrary page layouts or image-only text translatable.

## PDF conversion cannot find Poppler

Check the dependency:

```bash
bookforge doctor --pdf
```

Install Poppler command-line tools and put their binary directory on `PATH`, or
set `POPPLER_PATH` to that directory. On Windows, use a Poppler for Windows
release and point `POPPLER_PATH` at its `bin` directory.

Conversion can degrade when optional render/extraction tools are missing, so
read both `doctor --pdf` and the generated conversion report before
translating. Image-heavy, scanned, or unusual-layout PDFs may need OCR or manual
review even when conversion succeeds.

## Audiobook stitching or M4B assembly fails

Synthesis chunks are checkpointed independently, so a stitching failure does
not require you to pay for synthesis again. Install `ffmpeg` for `--stitch` and
both `ffmpeg` and `ffprobe` for chapterized `--m4b`, then rerun the same command.

Use a dry run to verify provider, voice, format, chapter count, and chunk plan:

```bash
bookforge audiobook book.epub --provider mock --dry-run
```

See [audiobooks.md](audiobooks.md) for provider-specific formats and limits.

## Cost estimates are unavailable or stale

Pricing comes from the bundled `pricing/providers.json`. A custom or newly
released model may not have a known price even though translation works.
Override the catalog per command or environment:

```bash
bookforge estimate book.epub --target Italian --pricing custom-pricing.json
```

Set `BOOKFORGE_PRICING_PATH` to use the same custom catalog across commands.
Treat estimates as planning values; provider billing is authoritative.

## What is safe to share

Do not publish `.bookforge/` or generated review artifacts. They can contain:

- the uploaded or snapshotted book;
- full source and translated text;
- provider/model settings and event logs;
- human review notes and corrections.

BookForge does not persist dashboard-pasted API keys, but redact commands,
screenshots, URLs, reports, and logs before attaching them to an issue. Never
include an API key or a copyrighted book in a public bug report.

