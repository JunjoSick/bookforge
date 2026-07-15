# Translating Into Toki Pona

Toki Pona is a first-class target in both the CLI (`--target "Toki Pona"`) and
the browser dashboard. Selecting it automatically activates BookForge's
built-in Toki Pona contract: concrete restatement without omission, short
sentences, established vocabulary, Toki Pona grammar, consistent tokiponized
names, and preservation of the author's stance and rhetorical intent. The
contract is captured in the job snapshot, so resume uses the exact same prompt.

## What you need

- Your own EPUB of the source book. BookForge never downloads books. If you
  only have a PDF, ingest it first with `bookforge convert book.pdf` (see
  [EPUB_PIPELINE.md](EPUB_PIPELINE.md)).
- A provider API key, unless you are doing a mock dry run.
- Optionally, the editable Toki Pona style sheet shipped in this repo:
  [styles/toki-pona.style.toml](styles/toki-pona.style.toml). The built-in
  contract is sufficient to run; pass this file only when you want its extra
  conventions, then customize it for the book.

## Recommended run

Dry run first (no network, validates the book parses and rebuilds):

```bash
bookforge translate book.epub --source Italian --target "Toki Pona" \
  --provider mock --model mock-identity --out book.tok.dry.epub
```

Real run:

```bash
bookforge translate book.epub --source Italian --target "Toki Pona" \
  --provider deepseek --model deepseek-v4-flash --no-thinking \
  --book-id il-mondo-al-contrario \
  --glossary docs/glossaries/il-mondo-al-contrario.toki-pona.toml \
  --qa off \
  --double-check all --auto-correct --correction-rounds 1 \
  --validate-output \
  --out book.tok.epub
```

Notes on the choices:

- **Model.** Toki Pona competence varies a lot between models. Prefer the
  strongest model you can afford (`deepseek-v4-pro` over `-flash`, or a
  frontier model through the OpenRouter preset). Cheap models produce
  grammatical-looking output that quietly drops meaning.
- **QA choice.** The `--qa suspicious` heuristic flags
  segments whose character-length ratio falls outside 0.5–2.2×. Toki Pona
  can legitimately land outside that band, so suspicious mode is not a useful
  selector. Use `--qa off` for the deterministic first pass and the separate
  double-check/correction pass shown above, or use `--qa all` when budget is
  less important than a second review of every segment.
- **Built-in style cost.** The target-specific contract is injected into every
  batch prompt. This costs input tokens, but prevents the much larger cost of
  retranslating a book after vocabulary and name conventions drift.
- **Expansion-aware chunking.** The built-in style also plans for Toki Pona's
  high output expansion before the first request: 200-token source units,
  exactly 1 item per batch, a 20x initial output allowance, a 4,096-token
  minimum output budget, and no adaptive batch growth. A single-item generation
  that still fills its budget is retried up to three times with compact anti-repetition guidance
  instead of being pointlessly split. Explicit
  `--max-segment-tokens`, `--batch-target-tokens`, and `--batch-max-items`
  values override these defaults.
- **Names.** For a book with recurring people/places, also pin the
  tokiponized forms in a glossary (`--glossary names.toml`) so every batch
  renders them identically. The supplied
  [book glossary](glossaries/il-mondo-al-contrario.toki-pona.toml) provides
  editable title and author conventions for this project. The built-in style
  covers the policy; the glossary covers the specific terms.

## Reviewing

Toki Pona translation is inherently lossy and interpretive: the translator
model restates each sentence's core meaning rather than transposing it. The
side-by-side review UI (`bookforge review <job> --open`) is the right tool to
check that arguments survived; flag segments there and `bookforge retry` them
with `retry_guidance`.

After review, generate audio from the translated EPUB rather than from the
Italian source:

```bash
bookforge audiobook book.tok.epub \
  --instructions "Speak Toki Pona clearly and evenly; pronounce every vowel consistently." \
  --m4b
```
