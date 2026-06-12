# BookForge

BookForge is the EPUB translation engine that keeps the LLM away from your document structure. It parses EPUBs into validated JSON payloads, checkpoints every segment, preserves markup/footnotes/links, and rebuilds valid EPUBs.

I built this to translate books for my partner. It's MIT-licensed in case it's useful to you.

## Status

MVP functionality is implemented:

- EPUB inspect, parse, segment, and rebuild
- Plain, marker-safe, and run-preserving translation contracts
- Mock provider for deterministic tests
- OpenAI-compatible provider
- DeepSeek and OpenRouter presets
- Bounded parallel segment translation with `--concurrency`
- SQLite checkpoint store
- Resume and retry commands
- Status and tail commands for persisted jobs
- Segment-level cache reuse for compatible prior translations
- Static side-by-side review HTML with flag export/import
- QA reports in JSON and Markdown
- Optional LLM QA review pass
- Cost estimates for known provider/model pairs

## Install

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

## Commands

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
  --provider-preset openrouter-paid-fast \
  --ui progress \
  --out book.it.epub
```

Translate with a glossary:

```bash
cargo run -p bookforge-cli -- glossary import glossary.series.toml
cargo run -p bookforge-cli -- translate book.epub \
  --source English \
  --target Italian \
  --provider-preset openrouter-paid-fast \
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

Retry failed or review-needed segments:

```bash
cargo run -p bookforge-cli -- retry <job-id> --only failed
cargo run -p bookforge-cli -- retry <job-id> --only needs-review
cargo run -p bookforge-cli -- retry <job-id> --only all
```

Validate a translated EPUB and report:

```bash
cargo run -p bookforge-cli -- validate book.it.epub --report book.it.report.json
```

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

Known limitations: provider API keys are read from environment variables, and validation is intentionally pragmatic rather than a full EPUBCheck replacement.

## Benchmarks

Run the mock release smoke benchmark with:

```bash
scripts/bench-mock.sh
```

See `docs/benchmarks.md` for metrics to capture in real-provider runs.

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
cargo fmt
cargo test
cargo clippy --all-targets --all-features
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
```
