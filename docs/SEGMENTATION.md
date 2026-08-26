# Segmentation

Segments are stable, bounded translation units derived from the document IR. A segment must not cross a spine resource boundary. In run state and quality metrics, **segment** means the scheduler unit returned by `bookforge-core::segment::build_segments`: one scheduler segment becomes one row in the job store's `segments` table and is the unit of checkpointing, resume, retry, and `needs_review`.

## Blocks

The EPUB reader emits blocks for headings, paragraphs, list items, quotes, table cells/rows, captions, footnotes, and addressable stray text nodes. `pre` and `code` content becomes `BlockKind::Code`; those blocks remain in the rebuilt EPUB but are not sent to the model.

Each block has a stable section id, block id, DOM path, ordinal, text runs, inline markers, protected spans, and token estimate. Before grouping, segment construction recomputes the estimate from the text instead of trusting a reader-supplied value. The canonical estimator (`bookforge_core::token_estimate::estimate_tokens`) weights each character individually: characters of unspaced scripts (Han, Kana, Hangul) count approximately one model token each, while every other character — space-delimited-script letters, digits, punctuation, whitespace — counts one quarter of a token, reproducing the four-characters-per-token rule of thumb for English-style BPE tokenizers. Mixed-script text therefore follows its counted proportions instead of a dominant-class verdict. This covers CJK without a language-name list or reliance on `--source-lang`, and keeps Arabic and Hebrew (caseless but space-delimited) on the spaced-script side.

The same estimator is used for batch packing, scheduler context budgets, glossary selection packing, QA and double-check chunking, and cost previews; cache namespaces derived before this change are invalidated by `CACHE_KEY_SCHEMA_VERSION` v3.

Inline EPUB structure is represented in the source prose with short per-block markers:

```text
Hello <m1>formatted</m1> text <r1/> here.
```

Paired inline elements use `<mN>...</mN>`. Empty inline elements use `<rN/>`. Legacy markers such as `<m id="...">` and `<ref id="..."/>` are still parsed for compatibility, but newly generated segments use the short syntax.

## Segment Construction

`bookforge-core::segment::build_segments` walks blocks in book order and groups them under the configured token budget. Segments do not cross section/resource boundaries. Short headings can be grouped with the following block when they fit. The same script-derived estimate is used later for provider request batching and offline mock-provider usage, so segment and batch budgets use one unit.

The translation profile supplies that token budget. `bookforge translate` and `bookforge estimate` both default to `v1-fast` (currently 12,000 maximum source tokens per scheduler segment), and explicit `--profile`, `--max-segment-tokens`, and `--context-tokens` estimate options follow translation's precedence. Provider request batching happens after segmentation: a request may contain several scheduler segments, but it does not create or remove job-store segment rows.

Each segment records:

- source text and block payloads;
- context before/after summaries;
- required markers and protected spans;
- a checksum/source hash used by checkpointing and cache lookup.

The cache namespace includes segmentation settings, profile namespace, batch mode, prompt version, glossary/style/entity fingerprints, and the inline marker schema version. Marker schema version `4` corresponds to the short per-block marker syntax plus stable whitespace boundaries between adjacent inline markers.

## Validation report counts

Validation report schema 3 renames the former `bookforge_validators.segment_count` field to `default_segmentation_count`. It is a diagnostic re-segmentation of the EPUB being validated with `SegmentationConfig::default()`; the report also records that configuration's maximum segment and context token values. It is not the source job's persisted scheduler count and must not be used as the denominator for job quality metrics. Schema 2's `segment_count` remains accepted when reports are deserialized.
