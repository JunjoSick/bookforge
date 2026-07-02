# BookForge

BookForge is the EPUB translation engine that keeps the LLM away from your document structure. It parses EPUBs into validated JSON payloads, checkpoints every segment, preserves markup/footnotes/links, and rebuilds valid EPUBs.

I built this to translate books for my partner. It's MIT-licensed in case it's useful to you.

## Why BookForge

EPUB structure is program-owned. Models receive prose-only JSON payloads and
never see or regenerate raw XHTML. Inline markers, protected spans, package
metadata, resources, and document ordering are validated and reassembled by
deterministic Rust code.

That boundary is the point of the project: malformed model output can be
retried without asking another model to repair the book. The result is a
checkpointed translation workflow whose structure can be tested independently
of translation quality.

## Status

BookForge v2.0 is usable for EPUB translation, PDF-to-EPUB ingestion, and
local browser-based translation runs:

- EPUB inspect, parse, segment, and rebuild
- EPUBCheck-backed standalone and post-translation validation
- EPUBCheck-clean structural regression against a pinned nine-book Standard
  Ebooks corpus; see [docs/corpus.md](docs/corpus.md)
- Plain, marker-safe, and run-preserving translation contracts
- Mock provider for deterministic tests
- OpenAI-compatible provider
- DeepSeek and OpenRouter presets
- Ollama and llama.cpp local-model presets
- Bounded parallel segment translation with `--concurrency`
- SQLite checkpoint store
- Resume and retry commands
- Status and tail commands for persisted jobs
- Live monitoring: terminal dashboard (`watch` / `--ui tui`) and a local
  browser dashboard (`serve`) over a shared, replayable run-state layer
- Browser workflow for non-developers: upload an EPUB, pick languages,
  choose provider/model, estimate cost, start translation, review, validate,
  and retry from `http://127.0.0.1:8765`
- Local-only secret handling for browser runs: pasted provider keys stay in
  server memory for the session and are never written to disk or placed on the
  command line
- Segment-level cache reuse for compatible prior translations
- Static side-by-side review HTML with flag export/import
- QA reports in JSON and Markdown
- Optional LLM QA review pass
- Cost estimates for known provider/model pairs
- Externalized, overridable provider pricing

## Install And Setup

### For non-technical users

BookForge v2 ships prebuilt installers for macOS, Linux, and Windows. You do
not need Rust, Cargo, Git, Python, Node, or a source-code checkout.

**What you need before starting:**

- A computer running macOS, Windows, or Linux.
- An `.epub` book file.
- An API key for the provider you want to use, unless you are doing a mock dry
  run.
- About two minutes.

**Install on macOS or Linux**

1. Open **Terminal**.
2. Copy this whole line, paste it into Terminal, and press Enter:

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/JunjoSick/bookforge/releases/latest/download/bookforge-cli-installer.sh | sh
```

3. Close Terminal and open it again.
4. Type:

```bash
bookforge
```

**Install on Windows**

1. Open **PowerShell** from the Start menu.
2. Copy this whole line, paste it into PowerShell, and press Enter:

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/JunjoSick/bookforge/releases/latest/download/bookforge-cli-installer.ps1 | iex"
```

3. Close PowerShell and open it again.
4. Type:

```powershell
bookforge
```

Running `bookforge` with no extra words opens the local web app. If the browser
does not open automatically, go to:

```txt
http://127.0.0.1:8765
```

The dashboard is intentionally local-only. It binds to `127.0.0.1`, so it is
for the person sitting at this computer, not a public website.

If the terminal says `bookforge` is not found, the install probably worked but
the terminal did not refresh its command list. Close the terminal completely,
open a new one, and try `bookforge` again.

### First Translation In The Browser

1. Click **New translation**.
2. Choose an `.epub` file.
3. Pick the source language, or leave it blank to auto-detect.
4. Pick the target language from the list.
5. Choose a quality tier, or open **Advanced** to choose the provider and model.
6. If the provider needs an API key, paste it into the password field. The key
   is remembered only while this `bookforge serve` process is running.
7. Review the estimate and click **Start translation**.
8. Watch progress in the **Progress** screen.
9. When it finishes, use **Review** to compare source and translation, and
   **Validation** to check the output EPUB.
10. If segments fail or need review, use **Retry failed / needs-review**.

Outputs, uploads, checkpoints, review data, and validation reports are stored
locally under `.bookforge/` in the folder where you started BookForge. Do not
share `.bookforge/` if the book or translation is private.

To stop the web app, return to the terminal running `bookforge` and press
`Ctrl+C`. The translation data stays on disk and can be opened again later.

### API Key Setup

BookForge can use several providers:

- **DeepSeek**: use a DeepSeek API key. In the browser dashboard, choose the
  DeepSeek provider or a DeepSeek-backed quality tier.
- **OpenRouter**: use an OpenRouter API key. This is useful when you want to
  choose from many hosted models.
- **OpenAI-compatible**: use a server or hosted provider that exposes an
  OpenAI-style `/v1` API. In **Advanced**, enter the base URL and model ID.
- **Mock**: no API key, no network provider, useful for a dry run.

For browser-launched runs, pasting a key into the dashboard is the simplest
path. For terminal use, you can also set environment variables before starting
BookForge:

```bash
export DEEPSEEK_API_KEY=...
export OPENROUTER_API_KEY=...
export OPENAI_API_KEY=...
```

### Advanced Install From Cargo

Install from crates.io when you already have Rust installed:

```bash
cargo install bookforge-cli
```

Or build the current checkout:

```bash
cargo build --release
```

The binary is:

```bash
target/release/bookforge
```

For development, use:

```bash
cargo run -p bookforge-cli -- <command>
```

## CLI Quick Start

```bash
export DEEPSEEK_API_KEY=...
bookforge inspect book.epub
bookforge translate book.epub --target Italian --provider-preset deep-seek-paid --validate-output
```

Use `cargo run -p bookforge-cli --` in front of commands when running from a
source checkout. Provider preset names are shown by `bookforge translate
--help`.

## Commands

Convert a PDF to a translatable EPUB (requires [poppler](https://poppler.freedesktop.org/)
command-line tools on PATH, or `POPPLER_PATH` pointing at their bin
directory; on Windows use the poppler-windows release zip):

```bash
cargo run -p bookforge-cli -- convert paper.pdf --out paper.epub
```

The converter detects two-column layouts per page (scientific papers),
repairs hyphenated line breaks, joins paragraphs across pages, and maps
oversized fonts to headings. It prints a fidelity report comparing the
reconstructed text against the raw `pdftotext` baseline — check that
coverage number (and `inspect` on the result) before spending tokens.
Figures, tables-as-images, and low-confidence page fallbacks are
roadmap items (ROADMAP §9b, phases P2–P4); for image-heavy PDFs expect
text-only output for now.

Inspect an EPUB:

```bash
cargo run -p bookforge-cli -- inspect book.epub
```

The inspect output includes a text-coverage metric: the percentage of
visible body text that lands in translatable blocks. Files with low
coverage (text in unsupported markup such as bare `<div>`s) are listed
individually — that text would ship untranslated, so check coverage
before spending tokens on a full run.

Estimate tokens and approximate cost:

```bash
cargo run -p bookforge-cli -- estimate book.epub \
  --source English \
  --target Italian \
  --provider openrouter \
  --model deepseek/deepseek-v4-flash
```

Pricing is loaded from the bundled `pricing/providers.json`. Override it with
`--pricing custom.json` or `BOOKFORGE_PRICING_PATH`.

Translate with OpenRouter:

```bash
export OPENROUTER_API_KEY=sk-or-...

cargo run -p bookforge-cli -- translate book.epub \
  --source English \
  --target Italian \
  --provider openrouter \
  --model deepseek/deepseek-v4-flash \
  --concurrency 4 \
  --timeout-seconds 120 \
  --qa off \
  --out book.it.epub
```

Translate with the default fast profile:

```bash
cargo run -p bookforge-cli -- translate book.epub \
  --target Italian \
  --provider-preset open-router-paid-fast \
  --ui progress \
  --out book.it.epub
```

Translate with a glossary:

```bash
cargo run -p bookforge-cli -- glossary import glossary.series.toml
cargo run -p bookforge-cli -- translate book.epub \
  --source English \
  --target Italian \
  --provider-preset open-router-paid-fast \
  --book-id fellowship \
  --series-id lord-of-the-rings \
  --glossary glossary.series.toml \
  --glossary-budget-tokens 800 \
  --glossary-format json \
  --prompt-extra "Maintain a literary register." \
  --out book.it.epub
```

Check provider and storage health:

```bash
cargo run -p bookforge-cli -- doctor --storage
cargo run -p bookforge-cli -- doctor \
  --provider openrouter \
  --model google/gemini-2.5-flash-lite
```

Translate with DeepSeek:

```bash
export DEEPSEEK_API_KEY=...

cargo run -p bookforge-cli -- translate book.epub \
  --source English \
  --target Italian \
  --provider deepseek \
  --model deepseek-v4-flash \
  --concurrency 4 \
  --out book.it.epub
```

Use any OpenAI-compatible endpoint:

```bash
export OPENAI_API_KEY=...

cargo run -p bookforge-cli -- translate book.epub \
  --source English \
  --target Italian \
  --provider openai-compatible \
  --base-url https://api.example.com/v1 \
  --api-key-env OPENAI_API_KEY \
  --model provider/model \
  --timeout-seconds 120 \
  --out book.it.epub
```

Local Ollama and llama.cpp recipes are documented in
[docs/local-models.md](docs/local-models.md).

Resume a job:

```bash
cargo run -p bookforge-cli -- resume <job-id> --timeout-seconds 120
```

Generate a side-by-side review page:

```bash
cargo run -p bookforge-cli -- review <job-id> --open
```

Ingest exported review flags and mark bad translations for retry:

```bash
cargo run -p bookforge-cli -- ingest-flags <job-id> --flags flags.json
cargo run -p bookforge-cli -- retry <job-id> --only needs-review
```

Manage glossary terms:

```bash
cargo run -p bookforge-cli -- glossary list --language 'English->Italian'
cargo run -p bookforge-cli -- glossary add "Aragorn" "Aragorn" \
  --category person \
  --scope series \
  --scope-id lord-of-the-rings \
  --source-lang English \
  --target-lang Italian \
  --case-sensitive
cargo run -p bookforge-cli -- glossary export glossary.series.toml \
  --scope series \
  --scope-id lord-of-the-rings \
  --language 'English->Italian'
```

Inspect persisted job state and recent events:

```bash
cargo run -p bookforge-cli -- status <job-id>
cargo run -p bookforge-cli -- tail <job-id> --lines 40
```

Monitor a run live — in the terminal or in a browser. Both follow a job's
`events.jsonl` and fold it into the same shared run state, so a run started in
another process (or already finished) replays identically:

```bash
# Full-screen terminal dashboard (omit the id to pick from recent jobs).
# Press `r` to mark failed / needs-review segments for retry, `q` to quit.
cargo run -p bookforge-cli -- watch <job-id>

# Local web dashboard for non-developers. Running without a subcommand is the
# easy launch path; it opens the browser and binds 127.0.0.1 only.
bookforge

# Equivalent explicit form:
bookforge serve --open

# From a source checkout, this helper uses an installed/local binary when one is
# available and falls back to Cargo for developers.
./scripts/launch-dashboard.sh

# Equivalent direct command from a checkout:
cargo run -p bookforge-cli -- serve --open
```

Attach the terminal dashboard directly to a run you start with `--ui tui`.
`watch` and `serve` are default-on build features (`tui` / `serve`); build with
`--no-default-features` for a minimal binary without them.

Retry failed or review-needed segments:

```bash
cargo run -p bookforge-cli -- retry <job-id> --only failed
cargo run -p bookforge-cli -- retry <job-id> --only needs-review
cargo run -p bookforge-cli -- retry <job-id> --only all
```

Validate a translated EPUB and report:

```bash
cargo run -p bookforge-cli -- validate book.it.epub \
  --report book.it.validation.json
```

BookForge invokes EPUBCheck when it is available. Set
`BOOKFORGE_EPUBCHECK` to an executable, its containing directory, or an
`epubcheck.jar`. Missing EPUBCheck is reported as `status: unavailable` and is
non-fatal. Use `--strict-epubcheck` to make warnings fail validation.

## QA Modes

Translation always runs hard validators before committing a segment. The optional LLM QA pass is controlled with:

```bash
--qa off
--qa suspicious
--qa all
```

`off` is the default. Reports still include deterministic soft warnings such as changed URLs, changed numbers, suspicious length ratios, model commentary, and repeated text.

Two structural defaults to know about:

- `pre`/`code` blocks are never sent to the model. They are copied through
  to the output byte-for-byte, preserving internal whitespace.
- The sliding context window (`--context-window`, default 3) is
  best-effort: a segment uses whichever predecessors have already
  finished and never waits for them. Pass `--context-strict` to restore
  the v1.3 fence behavior, which guarantees a complete context block but
  serializes segments within the context scope.

## Checkpoints And Cache

Runtime state is stored in:

```txt
.bookforge/jobs.sqlite
```

That path is ignored by git. Segment translations are persisted as each segment completes. New jobs reuse compatible cached translations when the source hash, prompt version, provider, model, source language, and target language match.

Progress events can be written in every UI mode:

```bash
cargo run -p bookforge-cli -- translate book.epub \
  --target Italian \
  --provider mock \
  --model mock-prefix-target \
  --ui json \
  --progress-jsonl .bookforge/runs/example/events.jsonl
```

Review artifacts contain the full source and translated text of the book. They are written locally under `.bookforge/runs/<job-id>/review/`; treat them as private user data.

Known limitations: terminal commands read provider API keys from environment
variables, while the browser dashboard can also accept a session-only pasted
key. PDF ingestion currently prioritizes text reconstruction; complex figures
and tables may require review of the conversion report.

## Benchmarks

Run the mock release smoke benchmark with:

```bash
scripts/bench-mock.sh
```

See `docs/benchmarks.md` for metrics to capture in real-provider runs.

Run the pinned structural corpus with:

```bash
bash scripts/corpus-fetch.sh small
bash scripts/corpus-smoke.sh small
```

## Secrets And Local Tests

Do not commit API keys or ad hoc test books. The repository ignores:

```txt
test/
.bookforge/
.claude/
.codex
*.env
*.key
key.txt
```

For local OpenRouter testing, place the key outside tracked paths or export it directly:

```bash
export OPENROUTER_API_KEY=...
```

## Development Checks

```bash
cargo fmt --all --check
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -A clippy::too_many_arguments -D warnings
```

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for what's expected in issues
and pull requests, and the architectural invariants any change has to
respect.

## Repository Layout

```txt
crates/bookforge-core   IR, segmentation, shared config
crates/bookforge-epub   EPUB inspect/read/rebuild
crates/bookforge-llm    prompts, providers, scheduler, validators
crates/bookforge-llm/prompts  Versioned prompt templates
crates/bookforge-store  SQLite checkpoint store
crates/bookforge-cli    CLI commands and reports
docs/                   Architecture notes
pricing/                bundled provider/model pricing
tests/corpus/            pinned Standard Ebooks corpus manifest
```

BookForge remains a tool built for one reader and shared under MIT. Bug reports
should include the BookForge version, operating system, provider/model, and a
redacted validation or QA report where possible. The sequenced project plan is
in [docs/ROADMAP.md](docs/ROADMAP.md).
