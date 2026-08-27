// Public-but-`doc(hidden)` test surface: hostile-corpus / property harnesses
// in `tests/` need to drive `validate_archive_metadata`,
// `ArchiveReadBudget::read_entry`, and `read_archive_text` against tiny
// injected bounds instead of the production limits (which would require
// ≥64 MiB fixtures to reach limit territory). Runtime behaviour with
// `DEFAULT_ARCHIVE_LIMITS` is unchanged; see `archive_limits` module docs.
#[doc(hidden)]
pub mod archive_limits;
pub mod reader;
pub mod reflow;
pub(crate) mod util;
pub mod validate;
pub mod writer;

pub use reader::{
    EpubInspection, FileTextCoverage, TextCoverage, inspect_epub, read_epub, text_coverage,
};
pub use reflow::{ReflowMergeRecord, ReflowOptions, ReflowOutcome, ReflowReport, reflow_epub};
pub use validate::{
    EpubValidationIssue, EpubValidationReport, ValidationSeverity, validate_block_translations,
    validate_translated_epub,
};
pub use writer::{RebuildOptions, rebuild_epub, rebuild_epub_with_options};
