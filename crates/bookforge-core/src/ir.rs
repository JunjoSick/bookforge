use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BookId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SectionId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BlockId(pub String);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Book {
    pub source_path: Option<PathBuf>,
    pub id: BookId,
    pub format: BookFormat,
    pub metadata: Metadata,
    pub manifest: Vec<Resource>,
    pub spine: Vec<SpineItem>,
    pub sections: Vec<Section>,
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BookFormat {
    Epub,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Metadata {
    pub title: Option<String>,
    pub creators: Vec<String>,
    pub language: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resource {
    pub id: String,
    pub href: String,
    pub media_type: String,
    pub properties: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpineItem {
    pub idref: String,
    pub href: Option<String>,
    pub linear: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Section {
    pub id: SectionId,
    pub href: String,
    pub spine_index: usize,
    pub title: Option<String>,
    pub heading_level: Option<u8>,
    pub block_ids: Vec<BlockId>,
    pub prev: Option<SectionId>,
    pub next: Option<SectionId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Block {
    pub id: BlockId,
    pub section_id: SectionId,
    pub kind: BlockKind,
    pub dom_path: DomPath,
    pub text_runs: Vec<TextRun>,
    pub inline_marks: Vec<InlineMark>,
    pub protected_spans: Vec<ProtectedSpan>,
    pub token_estimate: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlockKind {
    Heading(u8),
    Paragraph,
    ListItem,
    Quote,
    TableCell,
    TableRow,
    Footnote,
    Caption,
    Code,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomPath(pub Vec<usize>);

/// Path component base for addressing text nodes. Element children are
/// indexed 0, 1, 2, ...; the N-th non-whitespace text node inside an
/// element is addressed as `TEXT_NODE_PATH_BASE + N` in the final path
/// component. The offset keeps text-node addresses from ever colliding
/// with element indices. Reader and writer must count the same set of
/// text nodes (non-whitespace after entity decoding) for paths to align.
pub const TEXT_NODE_PATH_BASE: usize = usize::MAX / 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextRun {
    pub id: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InlineMark {
    pub id: String,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtectedSpan {
    pub kind: ProtectedSpanKind,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProtectedSpanKind {
    Url,
    Email,
    Code,
    Math,
    Number,
    Filename,
    InternalAnchor,
    Citation,
    FootnoteReference,
}
