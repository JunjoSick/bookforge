# Bookforge

Bookforge is a Rust, CLI-first EPUB translation engine. It parses EPUB structure, builds semantic segments, sends only structured prose payloads to an LLM, validates responses, checkpoints segment results, and rebuilds a valid EPUB.

The program owns EPUB structure. The model only translates validated JSON payloads.

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
- Segment-level cache reuse for compatible prior translations
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

## Checkpoints And Cache

Runtime state is stored in:

```txt
.bookforge/jobs.sqlite
```

That path is ignored by git. Segment translations are persisted as each segment completes. New jobs reuse compatible cached translations when the source hash, prompt version, provider, model, source language, and target language match.

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

## Repository Layout

```txt
crates/bookforge-core   IR, segmentation, shared config
crates/bookforge-epub   EPUB inspect/read/rebuild
crates/bookforge-llm    prompts, providers, scheduler, validators
crates/bookforge-store  SQLite checkpoint store
crates/bookforge-cli    CLI commands and reports
prompts/                Versioned prompt templates
docs/                   Architecture notes
tests/fixtures/         Committed minimal fixture only
```
