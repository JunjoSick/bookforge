# toImplement.md — Greenfield Book Translation Engine

## Mission

Build a new EPUB-first AI book translation tool from scratch.

The existing `TranslateBooksWithLLMs` repository should be treated as a rough product prototype, not as an implementation foundation. Its useful ideas may be ported at the feature/specification level, but the codebase, architecture, Python stack, web server, checkpoint model, chunking strategy, and placeholder-heavy internals should not be carried forward.

The goal is not to “improve the repo”.

The goal is to build a clean, fast, robust, modern translation engine that solves the same problem properly.

---

## Core Position

Build from scratch.

Use the existing repo only as inspiration for:

- EPUB translation as the primary use case.
- Long document support.
- Multiple LLM providers.
- User-supplied API keys.
- DeepSeek / OpenAI-compatible endpoint support.
- Resume capability.
- Formatting preservation.
- Progress reporting.
- Configurable translation prompts.
- Optional bilingual output later.
- Optional local model support later.

Do **not** port:

- Python implementation.
- Flask / Socket.IO server.
- Existing chunking code.
- Existing checkpoint code.
- Existing placeholder machinery.
- Existing adapter pattern.
- Existing UI.
- Existing database schema.
- Existing “standard/fast mode” internals.
- Existing reconstruction logic.

The old project demonstrates what users want. It does not demonstrate how this should be engineered.

---

## Target Project Name

Working name:

```txt
bookforge
```

---

## Initial Product Goal

Build a CLI-first EPUB translation tool that can:

```txt
EPUB input
  -> parse EPUB structure
  -> build semantic document graph
  -> segment book into safe translation units
  -> translate segments in parallel
  -> validate model outputs
  -> checkpoint every segment
  -> resume interrupted jobs
  -> rebuild a valid EPUB
```

Initial provider target:

```txt
OpenAI-compatible APIs, especially DeepSeek
```

Later providers:

```txt
OpenAI
OpenRouter
Ollama
LM Studio
vLLM
Gemini
Mistral
Anthropic
```

Initial format target:

```txt
EPUB only
```

Do not begin with PDF, DOCX, SRT, TXT, or a GUI.

---

## Implementation Stack

Use Rust.

Suggested workspace layout:

```txt
bookforge/
  Cargo.toml

  crates/
    bookforge-core/
      src/
        lib.rs
        ir.rs
        segment.rs
        scheduler.rs
        qa.rs
        config.rs
        error.rs

    bookforge-epub/
      src/
        lib.rs
        reader.rs
        writer.rs
        opf.rs
        nav.rs
        xhtml.rs
        sectioning.rs
        dom_patch.rs

    bookforge-llm/
      src/
        lib.rs
        provider.rs
        openai_compatible.rs
        rate_limit.rs
        prompt.rs
        response.rs

    bookforge-store/
      src/
        lib.rs
        db.rs
        artifacts.rs
        migrations.rs

    bookforge-cli/
      src/
        main.rs
        commands/
          mod.rs
          inspect.rs
          estimate.rs
          translate.rs
          resume.rs
          retry.rs
          validate.rs

  prompts/
    translate_segment.v1.md
    translate_marker_safe.v1.md
    translate_run_preserving.v1.md
    qa_segment.v1.md

  docs/
    ARCHITECTURE.md
    EPUB_PIPELINE.md
    SEGMENTATION.md
    CHECKPOINTING.md
    PROVIDERS.md

  tests/
    fixtures/
      minimal.epub
      inline-formatting.epub
      footnotes.epub
      lists.epub
      tables.epub
      huge-paragraph.epub
```

Suggested Rust crates:

```txt
tokio
clap
serde
serde_json
thiserror
anyhow
tracing
tracing-subscriber
reqwest
rusqlite or sqlx
zip
quick-xml
html5ever or scraper
sha2
uuid
chrono
tempfile
```

Prefer simplicity. Do not over-abstract before the MVP works.

---

## Non-Negotiable Design Rules

### 1. The program owns structure

The LLM translates prose.

The program preserves:

- EPUB package structure.
- XHTML structure.
- CSS references.
- image references.
- internal anchors.
- footnote links.
- metadata.
- spine order.
- table/list structure.

Never trust the model to preserve raw HTML correctly.

---

### 2. No raw full-file translation

Never send a whole XHTML file to the model.

Always parse, segment, translate, validate, and patch.

---

### 3. No fragile global placeholder soup

The old repo’s placeholder strategy is too brittle.

Use structured translation contracts:

- plain text mode;
- marker-preserving mode;
- run-preserving mode.

The model output must be machine-validated before being committed.

---

### 4. Parallelism is core, not optional

Segments must be translatable independently.

The scheduler should support bounded concurrency from the start.

Parallel completion order must not affect final output.

---

### 5. Checkpoint by segment identity, not index only

Every segment needs:

- stable ID;
- source checksum;
- section ID;
- block range;
- prompt version;
- provider;
- model.

Resume should reuse valid completed segment translations.

---

### 6. Failed output must be visible

Do not silently produce corrupted output.

A segment can be:

```txt
queued
in_flight
succeeded
failed
retry_pending
needs_review
skipped_cached
```

If a segment cannot be safely translated, preserve source text and mark it as `needs_review`.

---

## What To Port From Existing Repo

Port only the useful product concepts.

### Useful concept: provider flexibility

The old tool supports multiple model providers. Keep that product goal.

Implement a clean provider trait instead of copying the old provider code.

```rust
#[async_trait]
pub trait LlmProvider {
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse>;
    fn capabilities(&self) -> ProviderCapabilities;
}
```

Initial provider:

```txt
OpenAI-compatible
```

DeepSeek should be a preset over OpenAI-compatible.

---

### Useful concept: resumability

The old repo has checkpointing. Keep the idea, replace the implementation.

New checkpointing should be segment-level and content-addressed.

---

### Useful concept: preserving formatting

Keep the user-facing goal.

Reject the old implementation strategy.

Formatting preservation should happen through DOM patching and validated inline markers, not by asking the model to preserve raw serialized HTML.

---

### Useful concept: configurable prompts

Keep prompt customization, but version prompt templates.

Prompt version must become part of the cache key.

---

### Useful concept: progress reporting

Keep progress reporting.

Expose progress through CLI output first.

Later, this same job state can power a Tauri UI.

---

## What Not To Port

Do not port:

```txt
Python source
Flask app
Socket.IO logic
existing chunker
existing placeholder renumbering
existing EPUB translator
existing checkpoint manager
existing database tables
existing file adapter system
existing web frontend
existing fast/standard mode implementation
```

The old project is a reference for desired behavior, not a code dependency.

---

## Internal Representation

Implement a proper document IR.

```rust
pub struct Book {
    pub id: BookId,
    pub format: BookFormat,
    pub metadata: Metadata,
    pub manifest: Vec<Resource>,
    pub spine: Vec<SpineItem>,
    pub sections: Vec<Section>,
    pub blocks: Vec<Block>,
}

pub struct Section {
    pub id: SectionId,
    pub href: String,
    pub spine_index: usize,
    pub title: Option<String>,
    pub heading_level: Option<u8>,
    pub block_ids: Vec<BlockId>,
    pub prev: Option<SectionId>,
    pub next: Option<SectionId>,
}

pub struct Block {
    pub id: BlockId,
    pub section_id: SectionId,
    pub kind: BlockKind,
    pub dom_path: DomPath,
    pub text_runs: Vec<TextRun>,
    pub inline_marks: Vec<InlineMark>,
    pub protected_spans: Vec<ProtectedSpan>,
    pub token_estimate: usize,
}

pub struct Segment {
    pub id: SegmentId,
    pub section_id: SectionId,
    pub ordinal: usize,
    pub block_ids: Vec<BlockId>,
    pub source: SegmentSource,
    pub context: SegmentContext,
    pub constraints: SegmentConstraints,
    pub checksum: String,
}
```

Block kinds:

```rust
pub enum BlockKind {
    Heading(u8),
    Paragraph,
    ListItem,
    Quote,
    TableCell,
    TableRow,
    Footnote,
    Caption,
    Code,
    Unknown,
}
```

Protected spans should include:

```txt
URLs
emails
code
math
numbers where exact preservation matters
filenames
internal anchor IDs
citations
footnote references
```

---

## EPUB Import

Implement `bookforge-epub::reader`.

The reader must:

1. Open EPUB ZIP.
2. Validate `mimetype`.
3. Parse `META-INF/container.xml`.
4. Locate OPF package.
5. Parse manifest.
6. Parse spine.
7. Parse nav/toc if present.
8. Parse XHTML spine resources.
9. Preserve all non-XHTML resources.
10. Build `Book` IR.

Public API:

```rust
pub fn read_epub(path: &Path) -> Result<Book>;
```

The reader must preserve enough information to rebuild the EPUB without flattening or simplifying it.

---

## Sectioning

Implement semantic sectioning.

Respect these boundaries:

1. EPUB spine item.
2. Existing `<section>` / `<article>` containers.
3. Headings: `h1`, `h2`, `h3`.
4. TOC/nav entries.
5. Page break markers.
6. Large structural containers.

A segment must not cross a spine file boundary.

Avoid splitting:

```txt
inside a paragraph
inside a sentence unless unavoidable
inside a list item
inside a table row
between heading and short following paragraph
between footnote marker and target footnote
```

Oversized fallback:

```txt
block
  -> paragraph
  -> sentence group
  -> clause-level split
  -> hard split only as last resort
```

Public API:

```rust
pub fn build_segments(book: &Book, config: &SegmentationConfig) -> Result<Vec<Segment>>;
```

Segment IDs must be stable across reruns when source content is unchanged.

---

## Translation Modes

### Mode A: Plain text

Use for simple blocks without significant inline structure.

Model output:

```json
{
  "segment_id": "seg_000123",
  "translation": "..."
}
```

---

### Mode B: Marker-preserving

Use for normal literary prose with inline formatting.

Input may contain markers like:

```txt
This is <m id="m0">important</m>.
```

Output must preserve markers:

```json
{
  "segment_id": "seg_000123",
  "blocks": [
    {
      "block_id": "b_000456",
      "translation": "Questo è <m id=\"m0\">importante</m>."
    }
  ]
}
```

Validator must check:

```txt
all markers present
no unknown markers
no duplicated markers
valid nesting
valid block IDs
valid segment ID
```

---

### Mode C: Run-preserving

Use for links, footnotes, technical content, or after marker-preserving failure.

Input:

```json
{
  "block_id": "b_000456",
  "runs": [
    {"id": "r0", "text": "This is "},
    {"id": "r1", "text": "important"},
    {"id": "r2", "text": "."}
  ]
}
```

Output:

```json
{
  "block_id": "b_000456",
  "translated_runs": [
    {"id": "r0", "text": "..."},
    {"id": "r1", "text": "..."},
    {"id": "r2", "text": "..."}
  ]
}
```

Run-preserving mode is less elegant but safer. Use it as fallback.

---

## Prompt Files

Create:

```txt
prompts/translate_segment.v1.md
prompts/translate_marker_safe.v1.md
prompts/translate_run_preserving.v1.md
prompts/qa_segment.v1.md
```

Every request must record:

```txt
prompt template
prompt version
provider
model
temperature
source checksum
timestamp
input token count if available
output token count if available
```

Changing the prompt version should invalidate cached segment translations.

---

## Provider Layer

Initial provider: OpenAI-compatible chat completions.

Required config:

```toml
[providers.deepseek]
kind = "openai-compatible"
base_url = "https://api.deepseek.com/v1"
api_key_env = "DEEPSEEK_API_KEY"
default_model = "deepseek-chat"
max_concurrency = 8
timeout_seconds = 120
```

Request structure:

```rust
pub struct CompletionRequest {
    pub system: String,
    pub user: String,
    pub response_format: ResponseFormat,
    pub temperature: f32,
    pub max_output_tokens: Option<u32>,
    pub metadata: RequestMetadata,
}
```

Response structure:

```rust
pub struct CompletionResponse {
    pub content: String,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub finish_reason: FinishReason,
    pub provider_latency_ms: u64,
    pub raw: serde_json::Value,
}
```

Do not hardcode API keys.

---

## Scheduler

Implement bounded async scheduling.

Required behavior:

```txt
max concurrent requests
provider rate limit awareness
retry with backoff
pause/resume
out-of-order completion
ordered reconstruction
per-segment status updates
```

Retry ladder:

```txt
Attempt 1: normal translation
Attempt 2: stricter marker-safe prompt
Attempt 3: split segment smaller
Attempt 4: run-preserving mode
Attempt 5: preserve source and mark needs_review
```

Default CLI settings:

```txt
--concurrency 4
--max-retries 3
--checkpoint-every 1
```

For DeepSeek/cloud APIs, allow higher concurrency:

```txt
--concurrency 8
--concurrency 12
```

---

## Storage

Use SQLite plus artifact directory.

Layout:

```txt
.bookforge/
  jobs.sqlite
  artifacts/
    sha256/
      ab/
        cd/
          <hash>
  jobs/
    <job-id>/
      manifest.json
      plan.json
      logs.jsonl
      output/
      scratch/
```

SQLite schema:

```sql
CREATE TABLE jobs (
  id TEXT PRIMARY KEY,
  input_hash TEXT NOT NULL,
  source_lang TEXT,
  target_lang TEXT,
  provider TEXT,
  model TEXT,
  status TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE segments (
  id TEXT PRIMARY KEY,
  job_id TEXT NOT NULL,
  section_id TEXT NOT NULL,
  ordinal INTEGER NOT NULL,
  source_hash TEXT NOT NULL,
  prompt_version TEXT NOT NULL,
  provider TEXT NOT NULL,
  model TEXT NOT NULL,
  status TEXT NOT NULL,
  attempts INTEGER NOT NULL DEFAULT 0,
  input_tokens INTEGER,
  output_tokens INTEGER,
  cost_estimate REAL,
  error TEXT,
  translated_hash TEXT,
  FOREIGN KEY(job_id) REFERENCES jobs(id)
);

CREATE TABLE translations (
  segment_id TEXT PRIMARY KEY,
  translated_text TEXT NOT NULL,
  provider TEXT NOT NULL,
  model TEXT NOT NULL,
  prompt_version TEXT NOT NULL,
  created_at TEXT NOT NULL,
  FOREIGN KEY(segment_id) REFERENCES segments(id)
);

CREATE TABLE qa_findings (
  id TEXT PRIMARY KEY,
  segment_id TEXT NOT NULL,
  severity TEXT NOT NULL,
  kind TEXT NOT NULL,
  message TEXT NOT NULL,
  FOREIGN KEY(segment_id) REFERENCES segments(id)
);
```

Resume rule:

```txt
If source_hash + prompt_version + provider + model are compatible:
    reuse translation
else:
    retranslate
```

---

## EPUB Rebuild

Implement deterministic EPUB rebuild.

Requirements:

```txt
patch translated text into original XHTML DOM
preserve non-XHTML resources unchanged
preserve manifest
preserve spine
preserve nav/toc
preserve CSS/image/font paths
preserve anchors
preserve footnote links
write mimetype first and uncompressed
compress remaining files
validate output XHTML
```

Public API:

```rust
pub fn rebuild_epub(
    book: &Book,
    translations: &[SegmentTranslation],
    output: &Path,
) -> Result<()>;
```

Do not create a simplified EPUB.

Do not flatten the book.

Do not discard resources.

---

## QA

Hard validators:

```txt
valid JSON response
segment_id matches
block IDs match
markers preserved
no duplicated markers
no unknown markers
valid marker nesting
protected spans preserved
rebuilt XHTML parses
```

Soft validators:

```txt
suspicious source/target length ratio
large untranslated source fragments
numbers changed unexpectedly
URLs changed unexpectedly
footnote anchors moved suspiciously
model returned commentary
repetition / degeneration
glossary inconsistency
```

Output:

```txt
report.json
report.md
```

Report should include:

```txt
total segments
successful segments
cached segments
retried segments
failed segments
needs_review segments
input tokens
output tokens
estimated cost
QA warnings
output path
```

---

## CLI

Required commands:

```bash
bookforge inspect book.epub

bookforge estimate book.epub \
  --source English \
  --target Italian \
  --provider deepseek \
  --model deepseek-chat

bookforge translate book.epub \
  --source English \
  --target Italian \
  --provider deepseek \
  --model deepseek-chat \
  --concurrency 8 \
  --out book.it.epub

bookforge resume <job-id>

bookforge retry <job-id> --only failed

bookforge validate book.it.epub
```

DeepSeek alias:

```bash
--provider deepseek
```

expands to:

```txt
kind = openai-compatible
base_url = https://api.deepseek.com/v1
api_key_env = DEEPSEEK_API_KEY
```

Also support generic OpenAI-compatible config:

```bash
bookforge translate book.epub \
  --source English \
  --target Italian \
  --provider openai-compatible \
  --base-url https://api.deepseek.com/v1 \
  --api-key-env DEEPSEEK_API_KEY \
  --model deepseek-chat
```

---

## MVP Scope

MVP includes:

```txt
Rust workspace
CLI
EPUB import
EPUB sectioning
semantic segmentation
OpenAI-compatible provider
DeepSeek preset
parallel translation
segment checkpointing
resume
retry failed
EPUB rebuild
QA report
mock provider tests
```

MVP excludes:

```txt
PDF
DOCX
TXT
SRT
Flask
web server
Tauri UI
bilingual EPUB
translation memory UI
glossary editor
model comparison UI
batch mode
section refinement pass
```

---

## Milestones

### Milestone 1 — Workspace + CLI shell

Deliver:

```txt
Rust workspace
bookforge-cli crate
command parser
logging
basic error handling
```

Acceptance:

```bash
cargo build
cargo test
bookforge --help
```

---

### Milestone 2 — EPUB inspection

Deliver:

```txt
ZIP reader
mimetype validation
container.xml parser
OPF parser
manifest parser
spine parser
basic XHTML loader
```

Acceptance:

```bash
bookforge inspect tests/fixtures/minimal.epub
```

prints:

```txt
title
spine count
manifest count
XHTML count
nav/toc status
resource count
```

---

### Milestone 3 — IR + block extraction

Deliver:

```txt
Book IR
Section IR
Block IR
TextRun IR
InlineMark IR
DOM paths
protected spans
```

Acceptance:

```bash
bookforge inspect book.epub --structure
```

prints:

```txt
section count
block count by kind
estimated token count
```

---

### Milestone 4 — Segmentation

Deliver:

```txt
SegmentationConfig
build_segments()
stable segment IDs
source checksum
context generation
token estimate
```

Acceptance:

```bash
bookforge inspect book.epub --segments
```

prints ordered segments without crossing spine boundaries.

---

### Milestone 5 — Mock provider + scheduler

Deliver:

```txt
LlmProvider trait
mock provider
bounded async scheduler
out-of-order completion handling
ordered result assembly
```

Acceptance:

```bash
bookforge translate book.epub \
  --provider mock \
  --target Italian \
  --out mock.it.epub
```

works without real API calls.

---

### Milestone 6 — Storage + resume

Deliver:

```txt
SQLite job store
segment status table
translation table
logs.jsonl
resume command
retry command
```

Acceptance:

```txt
interrupt translation
resume job
completed segments are not retranslated
failed segments can be retried
```

---

### Milestone 7 — OpenAI-compatible provider

Deliver:

```txt
OpenAI-compatible chat completion client
DeepSeek preset
timeouts
rate-limit handling
basic token accounting
```

Acceptance:

```bash
bookforge translate book.epub \
  --provider deepseek \
  --model deepseek-chat \
  --source English \
  --target Italian \
  --concurrency 8 \
  --out book.it.epub
```

works when `DEEPSEEK_API_KEY` is set.

---

### Milestone 8 — Validation + retry ladder

Deliver:

```txt
JSON response validation
marker validation
protected span validation
normal retry
marker-safe retry
run-preserving fallback
needs_review fallback
```

Acceptance:

```txt
bad model output is rejected
missing markers trigger retry
unfixable segments become needs_review
```

---

### Milestone 9 — EPUB rebuild

Deliver:

```txt
DOM patching
XHTML serialization
EPUB repackaging
resource preservation
output validation
```

Acceptance:

```txt
output EPUB opens
XHTML parses
images/CSS survive
links survive
spine order preserved
```

---

### Milestone 10 — QA report

Deliver:

```txt
report.json
report.md
CLI summary
```

Example CLI summary:

```txt
Translated: 382/390 segments
Cached: 0
Retried: 17
Needs review: 8
Input tokens: 184,203
Output tokens: 201,902
Output: book.it.epub
Report: book.it.report.md
```

---

## Testing Strategy

Use mock providers heavily.

Mock provider modes:

```txt
identity
uppercase
prefix target language
random delay
malformed JSON
missing marker
duplicated marker
timeout
rate limit
```

Required tests:

```txt
EPUB parser unit tests
sectioning tests
segmentation tests
marker validator tests
provider response parser tests
scheduler concurrency tests
checkpoint/resume tests
EPUB rebuild tests
```

Do not depend on real LLM calls in CI.

---

## Final MVP Success Criteria

MVP is complete when:

```txt
1. A normal EPUB can be inspected.
2. A normal EPUB can be segmented.
3. Segments can be translated in parallel with a mock provider.
4. Segment results are checkpointed.
5. Interrupted jobs can resume.
6. Failed segments can be retried.
7. OpenAI-compatible provider works.
8. DeepSeek preset works.
9. EPUB output is rebuilt.
10. Output EPUB preserves resources and structure.
11. QA report identifies warnings/failures.
12. CI tests cover parser, segmentation, scheduler, checkpointing, provider parsing, and rebuild.
```

---

## Final Architecture Summary

Old prototype architecture:

```txt
file
  -> chunks
  -> sequential LLM calls
  -> fragile placeholders
  -> reconstructed output
```

New architecture:

```txt
EPUB
  -> document graph
  -> semantic segments
  -> parallel validated translations
  -> deterministic DOM patch
  -> valid EPUB rebuild
```

Build the second one from scratch.
