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

/// Formatting-control characters that shape display but carry no
/// translatable content. Poppler's text pipeline leaks them freely and
/// asymmetrically between `pdftotext` and `pdftohtml -xml` (PDF-7):
/// shapers emit zero-width joiners/non-joiners around Arabic ligatures
/// and directional marks that reconstruction legitimately reorders or
/// drops. Counting them as content made RTL pages swing below the 95%
/// coverage threshold and triggered OCR spend purely because of their
/// script.
pub(crate) fn is_invisible_formatting(ch: char) -> bool {
    matches!(ch as u32,
        0x00AD             // soft hyphen
        | 0x061C           // Arabic letter mark
        | 0x180E           // Mongolian vowel separator
        | 0x200B..=0x200F  // ZWSP, ZWNJ, ZWJ, LRM, RLM
        | 0x202A..=0x202E  // bidi embedding controls
        | 0x2060..=0x2064  // word joiner, invisible plus/minus/hyphen
        | 0x2066..=0x2069  // bidi isolate initiators/PDI
        | 0xFEFF           // BOM / zero-width no-break space
        | 0xFFF9..=0xFFFB  // interlinear annotation anchors
    )
}

/// Non-whitespace characters a translator would actually see: both
/// whitespace and formatting-control characters are excluded so every
/// coverage comparison (per-page and document totals) weighs the same
/// repertoire on both sides of the ratio.
pub(crate) fn count_visible_chars(text: &str) -> usize {
    text.chars()
        .filter(|ch| !ch.is_whitespace() && !is_invisible_formatting(*ch))
        .count()
}

impl Fragment {
    pub fn right(&self) -> i32 {
        self.left + self.width
    }

    pub fn char_count(&self) -> usize {
        self.spans
            .iter()
            .map(|span| count_visible_chars(&span.text))
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
            .map(|span| count_visible_chars(&span.text))
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
            .map(|span| count_visible_chars(&span.text))
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
