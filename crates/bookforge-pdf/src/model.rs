//! Page/line intermediate representation produced by the poppler XML
//! parser and consumed by reconstruction. Coordinates are pdftohtml's
//! integer pixel units, top-left origin.

/// A styled run of text within a line fragment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
}

/// One `<text>` fragment from pdftohtml, already a visual line or part
/// of one.
#[derive(Debug, Clone, PartialEq)]
pub struct Fragment {
    pub top: i32,
    pub left: i32,
    pub width: i32,
    pub height: i32,
    pub font: u32,
    pub spans: Vec<Span>,
}

/// A positioned image anchor from `pdftohtml -xml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRegion {
    pub top: i32,
    pub left: i32,
    pub width: i32,
    pub height: i32,
    pub src: Option<String>,
}

impl ImageRegion {
    pub fn bottom(&self) -> i32 {
        self.top + self.height
    }
}

impl Fragment {
    pub fn right(&self) -> i32 {
        self.left + self.width
    }

    pub fn char_count(&self) -> usize {
        self.spans
            .iter()
            .map(|span| span.text.chars().filter(|ch| !ch.is_whitespace()).count())
            .sum()
    }
}

/// A merged visual line (one or more fragments at the same height).
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub top: i32,
    pub left: i32,
    pub right: i32,
    pub height: i32,
    pub font_size: u32,
    pub spans: Vec<Span>,
}

impl Line {
    pub fn width(&self) -> i32 {
        self.right - self.left
    }

    pub fn text(&self) -> String {
        spans_text(&self.spans)
    }

    pub fn char_count(&self) -> usize {
        self.spans
            .iter()
            .map(|span| span.text.chars().filter(|ch| !ch.is_whitespace()).count())
            .sum()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub number: u32,
    pub width: i32,
    pub height: i32,
    pub fragments: Vec<Fragment>,
    pub images: Vec<ImageRegion>,
    /// font id -> point size, from `<fontspec>` declarations.
    pub font_sizes: std::collections::HashMap<u32, u32>,
}

/// Column handling requested on the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColumnMode {
    #[default]
    Auto,
    Single,
    Two,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LowConfidenceMode {
    Preserve,
    #[default]
    Linearize,
}

/// A reconstructed, reading-ordered document block ready for XHTML
/// emission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocBlock {
    Heading {
        level: u8,
        spans: Vec<Span>,
    },
    Paragraph {
        spans: Vec<Span>,
    },
    Figure {
        image: ImageAsset,
        caption: Option<Vec<Span>>,
    },
}

impl DocBlock {
    pub fn spans(&self) -> &[Span] {
        match self {
            DocBlock::Heading { spans, .. } => spans,
            DocBlock::Paragraph { spans } => spans,
            DocBlock::Figure {
                caption: Some(spans),
                ..
            } => spans,
            DocBlock::Figure { caption: None, .. } => &[],
        }
    }

    pub fn text(&self) -> String {
        spans_text(self.spans())
    }

    pub fn char_count(&self) -> usize {
        self.spans()
            .iter()
            .map(|span| span.text.chars().filter(|ch| !ch.is_whitespace()).count())
            .sum()
    }
}

pub(crate) fn spans_text(spans: &[Span]) -> String {
    spans
        .iter()
        .map(|span| span.text.as_str())
        .collect::<String>()
}

pub(crate) fn normalize_text_key(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageAsset {
    pub id: String,
    pub href: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
    pub page: u32,
    pub top: Option<i32>,
    pub left: Option<i32>,
    pub width: Option<i32>,
    pub height: Option<i32>,
}
