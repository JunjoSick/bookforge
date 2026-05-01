use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    BookforgeError, Result,
    config::SegmentationConfig,
    ir::{Block, BlockId, BlockKind, Book, Section, SectionId},
};

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
    InFlight,
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
    fn rejects_zero_token_limit() {
        let book = book_with_two_sections();
        let config = SegmentationConfig {
            max_segment_tokens: 0,
            context_tokens: 0,
        };

        assert!(build_segments(&book, &config).is_err());
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
