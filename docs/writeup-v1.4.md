# BookForge: Translating EPUBs Without Letting the LLM Near the Structure

> **Historical snapshot:** this draft describes the v1.x-era implementation
> and its batch contract; treat current `docs/` files as authoritative.

Draft note refreshed after the v1.5 extraction and scheduling work. The file
name is historical; the details below describe the current implementation.

## The Problem

The naive way to translate an EPUB with an LLM is to unzip the book, send each
XHTML file to a model, ask for XHTML back, then zip it again. That works until
the model drops a footnote anchor, rewrites an `href`, normalizes a structural
attribute, truncates a long chapter, or translates the chapter title in the
body but leaves the table of contents behind.

Those failures all come from the same mistake: the model is being asked to
translate prose and preserve arbitrary document structure at the same time.
BookForge keeps those responsibilities separate.

## The Invariant

BookForge's central invariant is:

> Rust owns EPUB structure. The model translates prose payloads only.

In practice:

- The EPUB reader parses package metadata, NCX labels, XHTML head titles, and
  visible body text into stable IR blocks.
- Inline formatting, links, footnote references, anchors, and empty inline
  elements become marker tokens inside the prose.
- The model receives JSON translation payloads and returns JSON translations.
- Validators check JSON shape, marker preservation, protected spans, and other
  deterministic constraints.
- The writer patches translated strings back into the original OPF, NCX, and
  XHTML resources instead of serializing a brand-new document tree.

The model never creates an EPUB id, `href`, spine item, package entry, or XML
attribute. It only moves words between languages.

## Markers

Inline structure is represented with short per-block markers:

```text
Hello <m1>formatted</m1> text <r1/> here.
```

Paired inline elements use `<mN>...</mN>`. Empty inline elements use `<rN/>`.
The marker ids are local to a block, which keeps prompts shorter than the old
`<m id="...">` form. The parser still accepts legacy markers for stored jobs
and compatibility tests, but new extraction emits the short form.

The contract is strict: if the source text has a marker, the translated text
must preserve that marker with the same tag name and valid nesting. If the
model drops, renames, duplicates, or invents markers, the response fails
validation.

## Translation Contracts

There are plain, marker-safe, and run-preserving contracts, each with batch and
compact variants. The runtime prompt contract version is `v2` for single
translation and `batch_v2` for batch translation, and the translation template
files carry matching `.v2.md` names.

The default CLI profile is the fast v1 profile. It enables batching, compact
prompting where appropriate, adaptive batch sizing, and the provider/runtime
knobs needed for high-throughput translation. The older balanced profile is
still available explicitly.

## Metadata And TOC

BookForge now treats several non-body strings as normal translatable blocks:

- OPF metadata titles such as `dc:title`;
- NCX `docTitle` and navigation `text` labels;
- XHTML `head/title`;
- visible body text in spine XHTML.

Those blocks enter the same segmentation, cache, checkpoint, validation, and
writer paths as body prose. If the same source string appears in both a chapter
heading and a TOC label, normal cache behavior makes the second translation
cheap and consistent.

## Rebuild

The writer does not ask a second model to fill XML around the translation. It
copies the original EPUB entries and patches only the resources that contain
translated block IDs. Patch routing uses section hrefs and DOM paths captured
by the reader.

This is why the reader/writer counting rules are strict. Element child indices,
addressable text-node indices, and entity-reference handling must match on both
sides or a translation could land in the wrong place. The roundtrip tests are
designed to catch those mistakes.

## Checkpointing And Resume

Every job is stored in `.bookforge/jobs.sqlite`. A fresh translation snapshots
the input EPUB and resolved run settings, inserts segment records, scans the
cache, then runs the shared checkpointed execution engine.

`bookforge resume <job-id>` reads the stored snapshot instead of current CLI
defaults. It rebuilds the source IR, verifies pending segment IDs, rehydrates
sliding context from stored terminal translations, and translates only
resumable segments. Retry-pending segments bypass cache so a known-bad result
does not get reused.

Fresh translation and resume now share `commands/translate/engine.rs` for the
provider execution core. That engine owns the checkpoint writer lifecycle and
selects batch or single-segment execution from the resolved settings.

## Recovery And Review

Validation failures are not silently repaired into the EPUB. The scheduler
retries within the configured attempt budget. Batch mode can split or repair
invalid items. Optional fallback and double-check passes can be enabled. If a
segment still cannot be trusted, it is marked failed or needs-review and source
text is preserved.

Reports and review artifacts make those decisions visible. The review command
builds a local static page from the stored job, source snapshot, translations,
QA findings, and deterministic warnings. The review data lives under
`.bookforge/runs/<job-id>/review/` and should be treated as private book data.

## What The Design Buys

The system pays for translation work, not structure recovery. It can resume
after interruption, cache terminal segments, preserve unusual EPUB formatting,
translate metadata and TOC labels through the same path as body prose, and
report exactly which segments failed instead of producing a silently damaged
book.

The main tradeoff is that deterministic extraction and patching are harder
than handing a whole XHTML file to a model. That cost is intentional. Bugs in
Rust path accounting can be reproduced, tested, and fixed. Silent structural
changes from a model cannot be trusted at book scale.
