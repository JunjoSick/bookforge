# Handoff to Codex — v1.7 Bilingual output (2026-07-03)

Written by Claude Code on behalf of the maintainer. This is the next
roadmap milestone after v1.6 shipped in v2.1.0.

**The authoritative spec is `docs/ROADMAP.md` §9b (lines ~1987–2140) —
read it in full before writing code.** It defines the three modes, the
per-element-type append policy table, the default CSS, the reassembly
design, edge cases, and acceptance criteria. If a needed detail is
missing from §9b, stop and ask the maintainer rather than inventing it.
This handoff only adds workflow constraints and pointers.

## Non-negotiables (ROADMAP §1 + §9b.7)

- **Bilingual mode is a reassembly concern, not a model-output concern.**
  No new LLM contracts; the model still emits exactly one target string
  per segment. The reassembler in `bookforge-epub` splices source +
  target per `--mode`. If you find yourself changing prompts or
  `bookforge-llm` response parsing, you have gone off-spec.
- `--mode replace` remains the default and must be byte-identical to
  current behavior (acceptance §9b.12.4). The cache key must NOT change
  for replace mode; check whether mode needs to factor into the cache
  namespace at all (translations are mode-independent — the same cached
  target serves all three modes, which is the correct and desirable
  outcome; reassembly mode should NOT invalidate cache).
- Every inserted element carries `class="bookforge-translation"` and
  `lang="<target>"`; RTL targets (ar/he/fa) also get `dir="rtl"`.
- EPUBCheck-valid output in every mode (§9b.9). The corpus/EPUBCheck
  test infrastructure from v1.8 is in the repo — wire the new modes into
  it (see `tests/` and the corpus CI job).

## Where the work lives

- `crates/bookforge-epub/src/writer.rs` — the reassembly layer. The
  existing replace path patches translated text into the original event
  stream; append-block/append-text add sibling events instead of
  replacing. Study `render_marked_translation` and the block patching
  flow (`text_node_patch`) before designing.
- `crates/bookforge-cli` — `--mode`, `--bilingual-css`,
  `--bilingual-style`, `--bilingual-separator` flags per §9b.10, plumbed
  through translate/resume/retry (resume must remember the mode: store it
  in the jobs table like other run config; a migration will be needed).
- The web dashboard wizard (`serve.rs`) and `estimate` do NOT need
  bilingual support in this milestone — CLI only. Do not touch serve.rs.
- Default stylesheet injection: see how nav.xhtml/content.opf are
  generated for where a CSS asset belongs; keep it out of the way if the
  EPUB already has one (§9b.8 generation-suffix note is explicitly
  deferred — skip it).

## Working agreement

- Branch: `v1.7-bilingual-output` (already created and checked out).
- Follow §9b exactly; §9b.11 out-of-scope list is binding.
- Commit in logical units, conventional commits, tests with each commit.
- Full workspace check/test/clippy/fmt clean before each commit.
- Unit tests: per-element policy table cases (p, blockquote, li,
  headings, figcaption, table cells, code/pre exclusion, empty-block
  skip), lang/dir attributes, separator handling, replace-mode
  byte-identity regression.
- End-to-end: run a MOCK-provider bilingual translation of a fixture
  EPUB in both append modes and validate with the BookForge validators
  (+ EPUBCheck if available on PATH).
- Do not push; leave the branch ready for Claude's review pass.

## Review fix pass (added 2026-07-03 late, IN PROGRESS — three blocking findings)

Implementation landed as 37f5886 + lang-tag fix 962aaed. Review (Claude
inline + Codex `exec review`) confirmed three EPUBCheck-blocking bugs,
all in `crates/bookforge-epub/src/writer.rs`, ALL STILL UNFIXED:

1. **NCX stylesheet injection (writer.rs ~170-174).** In append modes the
   stylesheet `<link>` is injected into every patched non-OPF resource,
   including `toc.ncx`; NCX `<head>` only allows `<meta>`, so EPUBs with
   an NCX toc fail EPUBCheck. Fix: inject only into XHTML resources
   (check media-type/extension before the `inject_stylesheet_link` branch).
2. **Table `<caption>` gets a `<p>` sibling inside `<table>` (writer.rs
   ~1320).** `caption` is in the `SiblingParagraph` list for AppendBlock;
   the appended `<p>` lands directly inside `<table>` — invalid. Fix:
   captions take the inline-span treatment (append the span INSIDE the
   caption; HTML allows flow content there) in both append modes.
3. **Nested block children wrapped inside translation elements
   (writer.rs ~1469).** For `<blockquote><p>…</p></blockquote>` /
   `<li><p>…</p></li>`, preserved child `<p>` marker events get wrapped
   inside the new translation `<p>`/`<span>`, emitting `<p><p>…</p></p>`
   or `<span><p>…</p></span>`. Fix: flatten block-level elements out of
   the copied inline template when building the translation wrapper
   (keep inline markup only), or append inside the existing child block.

Then: add regression tests for all three (NCX fixture, table-caption
fixture, blockquote/li-with-child-p fixtures), full workspace
check/test/clippy/fmt, mock-provider e2e in both append modes with the
output XHTML inspected, THEN push branch + open PR to main.

## Acceptance (from §9b.12, condensed)

append-block and append-text produce spec-shaped siblings/spans with
class+lang; EPUBCheck passes; replace mode unchanged; glossary/context/
style still apply (they're upstream of reassembly — verify with one test
that a glossary term still injects in append-block mode); token usage
unchanged vs replace.
