use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    BookforgeError, Result,
    config::SegmentationConfig,
    ir::{Block, BlockId, BlockKind, Book, Section, SectionId},
};

/// Bumped when the cache key derivation changes incompatibly.
pub const CACHE_KEY_SCHEMA_VERSION: u32 = 2;
/// Bumped when Segment / SegmentBlock layout changes incompatibly.
pub const SEGMENT_SCHEMA_VERSION: u32 = 1;
/// Bumped when inline marker extraction (m/keep/ref) changes incompatibly.
/// v2: depth-anchored block closing, lazily anchored text blocks for
/// non-whitelist elements, addressable stray text nodes — block ordinals
/// and marker assignments differ from v1 on affected books.
pub const INLINE_MARKER_SCHEMA_VERSION: u32 = 2;

/// Compute a cache namespace that scopes lookups to a single set of
/// schema and segmentation parameters. Cached rows from a different
/// namespace are not eligible for reuse.
///
/// `style_fingerprint` and `entities_fingerprint` are opt-in mixins:
/// pass an empty string to preserve cache compatibility with runs that
/// didn't use the feature; pass a non-empty fingerprint when the
/// rendered prompt actually changes. The two slots use distinct domain
/// separators so a style fingerprint can never collide with an entity
/// fingerprint of the same content.
pub fn compute_cache_namespace(
    max_segment_tokens: usize,
    context_tokens: usize,
    profile: &str,
    batch_enabled: bool,
    prompt_version: &str,
    glossary_fingerprint: &str,
    style_fingerprint: &str,
    entities_fingerprint: &str,
) -> String {
    compute_cache_namespace_inner(
        CACHE_KEY_SCHEMA_VERSION,
        max_segment_tokens,
        context_tokens,
        profile,
        batch_enabled,
        prompt_version,
        Some(glossary_fingerprint),
        if style_fingerprint.is_empty() {
            None
        } else {
            Some(style_fingerprint)
        },
        if entities_fingerprint.is_empty() {
            None
        } else {
            Some(entities_fingerprint)
        },
    )
}

pub fn compute_cache_namespace_v1(
    max_segment_tokens: usize,
    context_tokens: usize,
    profile: &str,
    batch_enabled: bool,
    prompt_version: &str,
) -> String {
    compute_cache_namespace_inner(
        1,
        max_segment_tokens,
        context_tokens,
        profile,
        batch_enabled,
        prompt_version,
        None,
        None,
        None,
    )
}

fn compute_cache_namespace_inner(
    cache_key_schema_version: u32,
    max_segment_tokens: usize,
    context_tokens: usize,
    profile: &str,
    batch_enabled: bool,
    prompt_version: &str,
    glossary_fingerprint: Option<&str>,
    style_fingerprint: Option<&str>,
    entities_fingerprint: Option<&str>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(cache_key_schema_version.to_le_bytes());
    hasher.update(SEGMENT_SCHEMA_VERSION.to_le_bytes());
    hasher.update(INLINE_MARKER_SCHEMA_VERSION.to_le_bytes());
    hasher.update((max_segment_tokens as u64).to_le_bytes());
    hasher.update((context_tokens as u64).to_le_bytes());
    hasher.update(profile.as_bytes());
    hasher.update([batch_enabled as u8]);
    hasher.update(prompt_version.as_bytes());
    if let Some(glossary_fingerprint) = glossary_fingerprint {
        hasher.update(glossary_fingerprint.as_bytes());
    }
    if let Some(style_fingerprint) = style_fingerprint {
        hasher.update(b"|style|");
        hasher.update(style_fingerprint.as_bytes());
    }
    if let Some(entities_fingerprint) = entities_fingerprint {
        hasher.update(b"|entities|");
        hasher.update(entities_fingerprint.as_bytes());
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockTranslation {
    pub block_id: BlockId,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SegmentId(pub String);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Segment {
    pub id: SegmentId,
    pub section_id: SectionId,
    pub ordinal: usize,
    pub block_ids: Vec<BlockId>,
    pub source: SegmentSource,
    pub context: SegmentContext,
    pub metadata: SegmentMetadata,
    pub constraints: SegmentConstraints,
    pub checksum: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentSource {
    pub text: String,
    pub blocks: Vec<SegmentBlock>,
    pub token_estimate: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentBlock {
    pub block_id: BlockId,
    pub kind: String,
    pub text: String,
    pub text_runs: Vec<SegmentTextRun>,
    pub protected_spans: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentTextRun {
    pub id: String,
    pub text: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SegmentContext {
    pub before: Option<String>,
    pub after: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SegmentMetadata {
    pub book_title: Option<String>,
    pub section_title: Option<String>,
    pub section_index: usize,
    pub segment_index_in_section: usize,
    pub total_segments_in_section: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SegmentConstraints {
    pub preserve_markers: Vec<String>,
    pub preserve_spans: Vec<String>,
    pub max_tokens: usize,
}

pub fn block_kind_label(kind: BlockKind) -> &'static str {
    match kind {
        BlockKind::Heading(_) => "heading",
        BlockKind::Paragraph => "paragraph",
        BlockKind::ListItem => "list_item",
        BlockKind::Quote => "quote",
        BlockKind::TableCell => "table_cell",
        BlockKind::TableRow => "table_row",
        BlockKind::Footnote => "footnote",
        BlockKind::Caption => "caption",
        BlockKind::Code => "code",
        BlockKind::Unknown => "unknown",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SegmentStatus {
    Queued,
    Succeeded,
    Failed,
    RetryPending,
    NeedsReview,
    SkippedCached,
}

pub fn build_segments(book: &Book, config: &SegmentationConfig) -> Result<Vec<Segment>> {
    if config.max_segment_tokens == 0 {
        return Err(BookforgeError::InvalidInput(
            "max_segment_tokens must be greater than zero".to_string(),
        ));
    }

    let mut segments = Vec::new();

    for (section_index, section) in book.sections.iter().enumerate() {
        let section_blocks = section
            .block_ids
            .iter()
            .map(|block_id| {
                book.blocks
                    .iter()
                    .find(|block| &block.id == block_id)
                    .ok_or_else(|| {
                        BookforgeError::InvalidInput(format!(
                            "section '{}' references missing block '{}'",
                            section.id.0, block_id.0
                        ))
                    })
            })
            .collect::<Result<Vec<_>>>()?;

        let mut current = Vec::<&Block>::new();
        let mut current_tokens = 0usize;
        let section_segments_start = segments.len();

        for block in section_blocks {
            // pre/code content is layout and syntax, not prose: sending it
            // to the model both mistranslates it and destroys intentional
            // whitespace. Excluded blocks are never patched, so the
            // original markup survives rebuild byte-for-byte.
            if matches!(block.kind, BlockKind::Code) {
                continue;
            }
            let block_tokens = block.token_estimate.max(1);
            let should_flush = !current.is_empty()
                && current_tokens + block_tokens > config.max_segment_tokens
                && !should_keep_with_previous(&current, block);

            if should_flush {
                push_segment(
                    &mut segments,
                    book,
                    section,
                    section_index,
                    &current,
                    config,
                );
                current.clear();
                current_tokens = 0;
            }

            current.push(block);
            current_tokens += block_tokens;
        }

        if !current.is_empty() {
            push_segment(
                &mut segments,
                book,
                section,
                section_index,
                &current,
                config,
            );
        }

        let total_in_section = segments.len() - section_segments_start;
        for (offset, segment) in segments[section_segments_start..].iter_mut().enumerate() {
            segment.metadata.segment_index_in_section = offset;
            segment.metadata.total_segments_in_section = total_in_section;
        }
    }

    apply_context(&mut segments, config.context_tokens);

    Ok(segments)
}

fn push_segment(
    segments: &mut Vec<Segment>,
    book: &Book,
    section: &Section,
    section_index: usize,
    blocks: &[&Block],
    config: &SegmentationConfig,
) {
    let segment_blocks = blocks
        .iter()
        .map(|block| {
            let mut spans = block
                .protected_spans
                .iter()
                .map(|span| span.text.clone())
                .collect::<Vec<_>>();
            spans.sort();
            spans.dedup();
            SegmentBlock {
                block_id: block.id.clone(),
                kind: block_kind_label(block.kind).to_string(),
                text: block_text(block),
                text_runs: block
                    .text_runs
                    .iter()
                    .map(|run| SegmentTextRun {
                        id: run.id.clone(),
                        text: run.text.clone(),
                    })
                    .collect(),
                protected_spans: spans,
            }
        })
        .collect::<Vec<_>>();
    let source_text = segment_blocks
        .iter()
        .map(|block| block.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let checksum = stable_hash(&source_text);
    let ordinal = segments.len();
    let first_block = blocks
        .first()
        .map(|block| block.id.0.as_str())
        .unwrap_or("empty");
    let id = SegmentId(format!(
        "seg_{}_{}_{}",
        section.id.0,
        first_block,
        &checksum[..12]
    ));

    let mut preserve_spans = blocks
        .iter()
        .flat_map(|block| block.protected_spans.iter().map(|span| span.text.clone()))
        .collect::<Vec<_>>();
    preserve_spans.sort();
    preserve_spans.dedup();

    let mut preserve_markers = blocks
        .iter()
        .flat_map(|block| block.inline_marks.iter().map(|mark| mark.id.clone()))
        .collect::<Vec<_>>();
    preserve_markers.sort();
    preserve_markers.dedup();

    let token_estimate = blocks
        .iter()
        .map(|block| block.token_estimate.max(1))
        .sum::<usize>();

    let metadata = SegmentMetadata {
        book_title: book.metadata.title.clone(),
        section_title: section.title.clone(),
        section_index,
        segment_index_in_section: 0,
        total_segments_in_section: 0,
    };

    segments.push(Segment {
        id,
        section_id: section.id.clone(),
        ordinal,
        block_ids: blocks.iter().map(|block| block.id.clone()).collect(),
        source: SegmentSource {
            text: source_text,
            blocks: segment_blocks,
            token_estimate,
        },
        context: SegmentContext::default(),
        metadata,
        constraints: SegmentConstraints {
            preserve_markers,
            preserve_spans,
            max_tokens: config.max_segment_tokens,
        },
        checksum,
    });
}

fn apply_context(segments: &mut [Segment], context_tokens: usize) {
    if context_tokens == 0 {
        return;
    }

    let sources = segments
        .iter()
        .map(|segment| segment.source.text.clone())
        .collect::<Vec<_>>();

    for (index, segment) in segments.iter_mut().enumerate() {
        segment.context.before = index
            .checked_sub(1)
            .and_then(|previous| sources.get(previous))
            .map(|text| tail_words(text, context_tokens));
        segment.context.after = sources
            .get(index + 1)
            .map(|text| head_words(text, context_tokens));
    }
}

fn should_keep_with_previous(current: &[&Block], next: &Block) -> bool {
    let Some(previous) = current.last() else {
        return false;
    };

    matches!(previous.kind, crate::ir::BlockKind::Heading(_)) && next.token_estimate <= 80
}

fn block_text(block: &Block) -> String {
    block
        .text_runs
        .iter()
        .map(|run| run.text.as_str())
        .collect::<Vec<_>>()
        .join("")
}

fn stable_hash(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to string should not fail");
    }
    output
}

fn head_words(text: &str, max_words: usize) -> String {
    text.split_whitespace()
        .take(max_words)
        .collect::<Vec<_>>()
        .join(" ")
}

fn tail_words(text: &str, max_words: usize) -> String {
    let words = text.split_whitespace().collect::<Vec<_>>();
    let start = words.len().saturating_sub(max_words);
    words[start..].join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{
        BlockKind, BookFormat, BookId, DomPath, Metadata, Resource, Section, SpineItem, TextRun,
    };

    #[test]
    fn builds_stable_segments_without_crossing_sections() {
        let book = book_with_two_sections();
        let config = SegmentationConfig {
            max_segment_tokens: 10,
            context_tokens: 4,
        };

        let first = build_segments(&book, &config).expect("segments should build");
        let second = build_segments(&book, &config).expect("segments should be stable");

        assert_eq!(first.len(), 3);
        assert_eq!(first[0].id, second[0].id);
        assert_eq!(first[1].checksum, second[1].checksum);
        assert_eq!(first[0].section_id.0, "sec_000000");
        assert_eq!(first[1].section_id.0, "sec_000000");
        assert_eq!(first[2].section_id.0, "sec_000001");
        assert_eq!(first[2].block_ids, vec![BlockId("b_000003".to_string())]);
    }

    #[test]
    fn code_blocks_are_excluded_from_segments() {
        let mut book = book_with_two_sections();
        book.blocks[1].kind = BlockKind::Code;
        let config = SegmentationConfig {
            max_segment_tokens: 100,
            context_tokens: 0,
        };

        let segments = build_segments(&book, &config).expect("segments should build");
        let segmented_blocks: Vec<&str> = segments
            .iter()
            .flat_map(|segment| segment.block_ids.iter().map(|id| id.0.as_str()))
            .collect();

        assert!(
            !segmented_blocks.contains(&"b_000001"),
            "code block must not be segmented for translation"
        );
        assert!(segmented_blocks.contains(&"b_000000"));
        assert!(segmented_blocks.contains(&"b_000002"));
    }

    #[test]
    fn rejects_zero_token_limit() {
        let book = book_with_two_sections();
        let config = SegmentationConfig {
            max_segment_tokens: 0,
            context_tokens: 0,
        };

        assert!(build_segments(&book, &config).is_err());
    }

    #[test]
    fn cache_namespace_changes_when_segmentation_settings_change() {
        let a = compute_cache_namespace(1200, 160, "Balanced", false, "v1", "glossary:a", "", "");
        let b = compute_cache_namespace(1201, 160, "Balanced", false, "v1", "glossary:a", "", "");
        let c = compute_cache_namespace(1200, 160, "Balanced", true, "v1", "glossary:a", "", "");
        let d = compute_cache_namespace(
            1200,
            160,
            "Balanced",
            false,
            "batch_v1",
            "glossary:a",
            "",
            "",
        );
        let e = compute_cache_namespace(1200, 160, "Balanced", false, "v1", "glossary:a", "", "");
        let f = compute_cache_namespace(1200, 160, "Balanced", false, "v1", "glossary:b", "", "");

        assert_ne!(a, b, "max_segment_tokens must affect namespace");
        assert_ne!(a, c, "batch_enabled must affect namespace");
        assert_ne!(a, d, "prompt_version must affect namespace");
        assert_ne!(a, f, "glossary fingerprint must affect namespace");
        assert_eq!(a, e, "namespace is deterministic for identical inputs");
    }

    #[test]
    fn legacy_cache_namespace_v1_ignores_glossary_fingerprint() {
        let current_without_terms =
            compute_cache_namespace(1200, 160, "Balanced", false, "v1", "", "", "");
        let current_with_terms =
            compute_cache_namespace(1200, 160, "Balanced", false, "v1", "glossary:a", "", "");
        let legacy = compute_cache_namespace_v1(1200, 160, "Balanced", false, "v1");

        assert_ne!(legacy, current_without_terms);
        assert_ne!(legacy, current_with_terms);
        assert_eq!(
            legacy,
            compute_cache_namespace_v1(1200, 160, "Balanced", false, "v1")
        );
    }

    #[test]
    fn cache_namespace_is_stable_when_style_fingerprint_is_empty() {
        // Users who don't use --style must see no cache invalidation when
        // they upgrade to a build that supports style sheets.
        let without_style =
            compute_cache_namespace(1200, 160, "Balanced", false, "v1", "glossary:a", "", "");
        let still_without_style =
            compute_cache_namespace(1200, 160, "Balanced", false, "v1", "glossary:a", "", "");
        assert_eq!(without_style, still_without_style);
    }

    #[test]
    fn cache_namespace_changes_when_style_fingerprint_changes() {
        let baseline =
            compute_cache_namespace(1200, 160, "Balanced", false, "v1", "glossary:a", "", "");
        let with_style = compute_cache_namespace(
            1200,
            160,
            "Balanced",
            false,
            "v1",
            "glossary:a",
            "style:a",
            "",
        );
        let with_other_style = compute_cache_namespace(
            1200,
            160,
            "Balanced",
            false,
            "v1",
            "glossary:a",
            "style:b",
            "",
        );

        assert_ne!(
            baseline, with_style,
            "switching on a style sheet must invalidate cache"
        );
        assert_ne!(
            with_style, with_other_style,
            "different style fingerprints must yield different namespaces"
        );
    }

    #[test]
    fn cache_namespace_changes_when_entities_fingerprint_changes() {
        let baseline =
            compute_cache_namespace(1200, 160, "Balanced", false, "v1", "glossary:a", "", "");
        let with_entities = compute_cache_namespace(
            1200,
            160,
            "Balanced",
            false,
            "v1",
            "glossary:a",
            "",
            "entities:a",
        );
        let with_other_entities = compute_cache_namespace(
            1200,
            160,
            "Balanced",
            false,
            "v1",
            "glossary:a",
            "",
            "entities:b",
        );
        assert_ne!(
            baseline, with_entities,
            "switching on entities must invalidate cache"
        );
        assert_ne!(
            with_entities, with_other_entities,
            "different entity fingerprints must yield different namespaces"
        );
    }

    #[test]
    fn style_and_entities_fingerprints_use_distinct_domain_separators() {
        // The same hex string used as style vs. entities must not produce
        // the same namespace — domain separators prevent the collision.
        let as_style =
            compute_cache_namespace(1200, 160, "Balanced", false, "v1", "glossary:a", "ab", "");
        let as_entities =
            compute_cache_namespace(1200, 160, "Balanced", false, "v1", "glossary:a", "", "ab");
        assert_ne!(as_style, as_entities);
    }

    fn book_with_two_sections() -> Book {
        let section_a = SectionId("sec_000000".to_string());
        let section_b = SectionId("sec_000001".to_string());

        Book {
            source_path: None,
            id: BookId("test".to_string()),
            format: BookFormat::Epub,
            metadata: Metadata::default(),
            manifest: vec![Resource {
                id: "chapter".to_string(),
                href: "chapter.xhtml".to_string(),
                media_type: "application/xhtml+xml".to_string(),
                properties: Vec::new(),
            }],
            spine: vec![SpineItem {
                idref: "chapter".to_string(),
                href: Some("chapter.xhtml".to_string()),
                linear: true,
            }],
            sections: vec![
                Section {
                    id: section_a.clone(),
                    href: "chapter.xhtml".to_string(),
                    spine_index: 0,
                    title: Some("One".to_string()),
                    heading_level: Some(1),
                    block_ids: vec![
                        BlockId("b_000000".to_string()),
                        BlockId("b_000001".to_string()),
                        BlockId("b_000002".to_string()),
                    ],
                    prev: None,
                    next: Some(section_b.clone()),
                },
                Section {
                    id: section_b.clone(),
                    href: "chapter2.xhtml".to_string(),
                    spine_index: 1,
                    title: None,
                    heading_level: None,
                    block_ids: vec![BlockId("b_000003".to_string())],
                    prev: Some(section_a.clone()),
                    next: None,
                },
            ],
            blocks: vec![
                block("b_000000", &section_a, BlockKind::Heading(1), "One", 2),
                block(
                    "b_000001",
                    &section_a,
                    BlockKind::Paragraph,
                    "short lead",
                    3,
                ),
                block(
                    "b_000002",
                    &section_a,
                    BlockKind::Paragraph,
                    "this paragraph forces a second segment",
                    10,
                ),
                block(
                    "b_000003",
                    &section_b,
                    BlockKind::Paragraph,
                    "new section must stay separate",
                    4,
                ),
            ],
        }
    }

    fn block(
        id: &str,
        section_id: &SectionId,
        kind: BlockKind,
        text: &str,
        token_estimate: usize,
    ) -> Block {
        Block {
            id: BlockId(id.to_string()),
            section_id: section_id.clone(),
            kind,
            dom_path: DomPath(vec![0]),
            text_runs: vec![TextRun {
                id: "r0".to_string(),
                text: text.to_string(),
            }],
            inline_marks: Vec::new(),
            protected_spans: Vec::new(),
            token_estimate,
        }
    }
}
