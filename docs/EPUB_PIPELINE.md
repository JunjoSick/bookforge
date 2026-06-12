# EPUB Pipeline

The EPUB pipeline parses package metadata, preserves resources, translates XML text through the document IR, and rebuilds a valid EPUB deterministically.

## Read

`bookforge-epub::read_epub` opens the container, finds the package document, parses the OPF manifest/spine, and stores the original bytes for every resource. It extracts translatable blocks from:

- OPF metadata titles such as `dc:title`;
- NCX `text` labels, including `docTitle` and navigation labels;
- XHTML `head/title`;
- XHTML body content from known block elements and from visible direct text inside non-structural wrappers.

The reader suppresses `script`, `style`, `svg`, and `math`. It preserves package resources, stylesheets, images, fonts, and non-translated XML entries. Named and numeric entities are decoded for segmentation, while path accounting ignores entity events so reader and writer stay aligned.

## Inline Structure

Inline formatting, links, anchors, and empty inline elements are represented as marker tokens inside a block's prose. New extraction emits short per-block markers such as `<m1>...</m1>` and `<r1/>`. The marker parser also accepts the older verbose marker forms so stored jobs and tests can read legacy text.

## Rebuild

`bookforge-epub::rebuild_epub` does not serialize a new EPUB tree from scratch. It copies the original archive entries and patches only resources that have translated block IDs. The same patcher handles XHTML, OPF, and NCX XML resources.

Patch routing is by section href and DOM path. For ordinary element blocks, the writer replaces the captured element text while preserving attributes and child elements represented by markers. For addressable stray text nodes, the writer uses the same non-whitespace text-node counting rule as the reader.

After patching, XML is parsed again as a basic validity check. Missing translations leave source text in place; failed or needs-review segments are visible in the report and review flow.

