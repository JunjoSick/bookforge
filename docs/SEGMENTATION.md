# Segmentation

Segments are stable, bounded translation units derived from the document IR. A segment must not cross a spine resource boundary.

## Blocks

The EPUB reader emits blocks for headings, paragraphs, list items, quotes, table cells/rows, captions, footnotes, and addressable stray text nodes. `pre` and `code` content becomes `BlockKind::Code`; those blocks remain in the rebuilt EPUB but are not sent to the model.

Each block has a stable section id, block id, DOM path, ordinal, text runs, inline markers, protected spans, and token estimate. Inline EPUB structure is represented in the source prose with short per-block markers:

```text
Hello <m1>formatted</m1> text <r1/> here.
```

Paired inline elements use `<mN>...</mN>`. Empty inline elements use `<rN/>`. Legacy markers such as `<m id="...">` and `<ref id="..."/>` are still parsed for compatibility, but newly generated segments use the short syntax.

## Segment Construction

`bookforge-core::segment::build_segments` walks blocks in book order and groups them under the configured token budget. Segments do not cross section/resource boundaries. Short headings can be grouped with the following block when they fit.

Each segment records:

- source text and block payloads;
- context before/after summaries;
- required markers and protected spans;
- a checksum/source hash used by checkpointing and cache lookup.

The cache namespace includes segmentation settings, profile namespace, batch mode, prompt version, glossary/style/entity fingerprints, and the inline marker schema version. Marker schema version `3` corresponds to the short per-block marker syntax.

