# EPUB Pipeline

The EPUB pipeline parses package metadata, preserves resources, translates XML text through the document IR, and rebuilds a valid EPUB deterministically.

## Read

`bookforge-epub::read_epub` opens the container, finds the package document, parses the OPF manifest/spine, and stores the original bytes for every resource. It extracts translatable blocks from:

- OPF metadata titles such as `dc:title`;
- NCX `text` labels, including `docTitle` and navigation labels;
- XHTML `head/title`;
- XHTML body content from known block elements and from visible direct text inside non-structural wrappers.

Documents are extracted in this order: the spine, then the navigation document,
then every remaining manifest document of type `application/xhtml+xml` or
`text/html` (linked endnotes or appendices outside the reading order), in
manifest order, as `sec_resource_NNNNNN` sections. Each document is extracted
once, keyed by its normalized archive path, so a spine that repeats a document
or manifest items that alias one file do not duplicate prose. A non-spine
manifest entry whose file is missing is skipped with a warning. Generic
`application/xml` resources outside the spine are not treated as prose.
The text coverage that `bookforge inspect` reports covers the same content
documents, without the navigation document.

The reader suppresses `script`, `style`, `svg`, and `math`. When one of them
occurs inside a block, the whole subtree becomes one atomic reference marker
(`<rN/>`); the model never sees or translates its contents. The reader
preserves package resources, stylesheets, images, fonts, and non-translated XML
entries.

Adjacent XML text, entity references, and CDATA sections form one visible text
node. Named HTML5 and numeric entities are decoded; an unknown entity is kept
literally (`&name;`) rather than dropped. The reader and writer use the same
text-node boundaries, so loose text outside the known block elements is
addressed identically on both sides.

## PDF-derived chapter boundaries

`bookforge convert` writes one spine item (`chapter-NNNN.xhtml`) per chapter,
starting a chapter at each reconstructed level-1 heading. A chapter that would
hold nothing but headings, such as a half-title page ahead of the title page or
a part title ahead of its first chapter, merges into the next one, and a
heading-only tail merges into the one before, so no file is empty or
heading-only. When fewer than two chapters with body text remain (a paper whose
only level-1 heading is its title, say), the output keeps the single
`content.xhtml` spine item, byte for byte as before.

`--chapter-prefix <TEXT>` replaces the heading rule with a literal one.
Matching is case-insensitive against the literal prefix of a block's complete
whitespace-normalized visible text. It is not limited to blocks classified as
headings because noisy PDF extraction can reconstruct a visually obvious
chapter label as a paragraph.

In both modes, blocks before the first boundary become a front-matter spine
item, so title pages, introductions, and prefaces are not dropped; with the
heading rule, front matter holding only headings opens the first chapter
instead. Boundaries matching nothing use the single-file writer. More than 256
boundaries, or boundaries so dense that there are fewer than two text blocks
per boundary on average, also fall back to that writer and add a conversion
warning.

The navigation document lists each chapter file with that chapter's own h1–h3
headings nested under it. Heading ids (`head-NNNN`) are numbered across the
whole book rather than per file, so they are unique in the EPUB and match the
single-file numbering.

The resulting structural sections are scheduler boundaries: segments never
cross from one spine section into another. This removes single-chapter
structure as a confound when evaluating PDF-derived books, but does not by
itself establish or claim an improvement in translation quality.

## Inline Structure

Inline formatting, links, anchors, and empty inline elements are represented as marker tokens inside a block's prose. New extraction emits short per-block markers such as `<m1>...</m1>` and `<r1/>`. The marker parser also accepts the older verbose marker forms so stored jobs and tests can read legacy text.

The IR remains lossless even when converted EPUBs contain redundant wrappers such as `<span><span>text</span></span>`: both elements receive distinct markers because the original elements may carry different attributes. Batch prompt rendering uses a reversible projection for the narrower case where a paired marker's opening tag immediately follows its parent's opening tag and its closing tag immediately precedes the parent's closing tag. The model sees only the parent marker; BookForge restores every omitted child marker before validating or storing the translation. Nested markers with any text or marker content outside the child cover different ranges and are never collapsed.

This projection does not change segment source text, checksums, inline-marker schema, or the cache namespace. Existing cached translations already contain the complete marker tree and remain valid; fresh translations are expanded to that same representation before deterministic rebuild.

## Rebuild

`bookforge-epub::rebuild_epub` does not serialize a new EPUB tree from scratch. It copies the original archive entries and patches only resources that have translated block IDs. The same patcher handles XHTML, OPF, and NCX XML resources.

Patch routing is by section href and DOM path. For ordinary element blocks, the writer replaces the captured element text while preserving attributes and child elements represented by markers. For addressable stray text nodes, the writer uses the same non-whitespace text-node counting rule as the reader.

Before anything is written, the rebuild rejects a request it could only satisfy
by guessing: a translation for a block the book does not contain, two
translations for one block or one DOM path, a block whose section or document
is missing, a DOM path that matches nothing in its document, and duplicate
block or section ids in the IR. The rebuild then fails and the existing output
is left in place. A translation whose inline markers cannot be applied still
leaves that block's source content in place and is logged; the rebuild
succeeds.

After patching, XML is parsed again as a basic validity check. Blocks with no
supplied translation keep their source text; failed or needs-review segments
are visible in the report and review flow. The rebuilt archive is staged
beside the output and published with a single rename, so the output path
always holds either the previous or the new complete file.

## Validate

`bookforge validate` checks every `.xhtml`, `.html`, `.opf`, `.ncx`, `.xml` and
`.svg` entry (extensions match case-insensitively). Each must decode as UTF-8
or as BOM-marked UTF-16, parse without errors, and contain exactly one complete
root element. Every read goes through the same decompression budget as the
reader, so an entry whose declared size understates its real expansion is
rejected.

## Content audit

Structural validity does not establish that the prose was translated.
`scripts/epub_quality_audit.py SOURCE.epub TARGET.epub [--json REPORT.json]`
compares a source EPUB with a replace-mode translation paragraph by paragraph,
with no provider calls. It reports review candidates: unchanged long prose,
long copied source spans behind a translated prefix, empty targets, extreme
length ratios, repetition the source does not have, identical long targets for
different sources, and locations present on only one side. These are
candidates, not verdicts: quotations, names, and bibliographies can
legitimately stay unchanged. Its tests run with
`python -m unittest discover -s scripts -p test_epub_quality_audit.py`.

