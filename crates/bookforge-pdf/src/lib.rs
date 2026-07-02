//! PDF ingestion for BookForge (ROADMAP §9).
//!
//! Layout extraction is delegated to poppler's command-line tools;
//! everything after the `pdftohtml -xml` output is deterministic Rust:
//! line merging, column detection, reading order, paragraph clustering,
//! heading detection, and synthetic-EPUB assembly. The produced EPUB
//! flows through the ordinary BookForge pipeline — this crate is an
//! ingestion front-end, not a parallel translation path.

pub mod convert;
pub mod epub;
pub mod model;
pub mod parse;
pub mod reconstruct;
pub mod report;
pub mod tools;

pub use convert::{ConvertOptions, ConvertOutcome, convert_pdf};
pub use model::{ColumnMode, DocBlock, ImageAsset, ImageRegion, Line, Page, Span};
pub use parse::parse_pdf2xml;
pub use reconstruct::reconstruct;
pub use report::ConversionReport;
pub use tools::{PopplerTools, ToolError};

#[derive(Debug, thiserror::Error)]
pub enum PdfError {
    #[error("poppler tooling: {0}")]
    Tool(#[from] tools::ToolError),

    #[error("pdftohtml XML parse: {0}")]
    Xml(#[from] quick_xml::Error),

    #[error("invalid pdftohtml output: {0}")]
    InvalidInput(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("zip: {0}")]
    Zip(#[from] zip::result::ZipError),
}

pub type Result<T> = std::result::Result<T, PdfError>;
