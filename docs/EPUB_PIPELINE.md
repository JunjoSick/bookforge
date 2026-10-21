# EPUB Pipeline

The EPUB pipeline parses package metadata, preserves resources, translates XML text through the document IR, and rebuilds a valid EPUB deterministically.

## Read

`bookforge-epub::read_epub` opens the container, finds the package document, parses the OPF manifest/spine, and stores the original bytes for every resource. It extracts translatable blocks from:

- OPF metadata titles such as `dc:title`;
- NCX `text` labels, including `docTitle` and navigation labels;
- XHTML `head/title`;
- XHTML body content from known block elements and from visible direct text inside non-structural wrappers.

The reader suppresses `script`, `style`, `svg`, and `math`. It preserves package resources, stylesheets, images, fonts, and non-translated XML entries. Named and numeric entities are decoded for segmentation, while path accounting ignores entity events so reader and writer stay aligned.

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

After patching, XML is parsed again as a basic validity check. Missing translations leave source text in place; failed or needs-review segments are visible in the report and review flow.

