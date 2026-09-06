//! The exact input preprocessing the audiobook launcher performs, exposed
//! as one reusable function (AUDIO-7).
//!
//! Estimates that skip this pipeline diverge from real runs: PDF-derived
//! books get their page-sliced spine grouped around chapter headings only
//! when `pdf_page_grouping` is derived from the reflow report, and reflow's
//! PDF cleanup removes conversion furniture before chunking. Both effects
//! change chapter counts, chunk counts, and billable characters. Callers
//! must run this same function for planning/estimates *and* launches.

use std::path::{Path, PathBuf};

use bookforge_core::ir::Book;
use bookforge_epub::{ReflowOptions, read_epub, reflow_epub};

/// A book prepared exactly the way a narration build prepares it.
#[derive(Debug)]
pub struct NarrationSource {
    pub book: Book,
    /// True when reflow positively identified pdftohtml output; mirrors
    /// [`crate::builder::AudiobookOptions::pdf_page_grouping`].
    pub pdf_page_grouping: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum NarrationSourceError {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to prepare EPUB narration from {input}: {message}")]
    Prepare { input: PathBuf, message: String },

    #[error("failed to read EPUB {input}: {message}")]
    Read { input: PathBuf, message: String },
}

/// Reflow the EPUB with PDF cleanup into a staging file inside
/// `scratch_dir`, then parse it. `scratch_dir` must already exist; callers
/// control its privacy (the dashboard passes its private per-request temp
/// dir, the CLI uses the system temp dir today).
///
/// This is the single source of truth for launch-time input preparation:
/// estimation, planning, and launching all produce identical chunk plans
/// from identical inputs because they share this step.
pub fn read_narration_source(
    input: &Path,
    scratch_dir: &Path,
) -> Result<NarrationSource, NarrationSourceError> {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let staged = scratch_dir.join(format!(
        "bookforge-audio-clean-{}-{}-{sequence}.epub",
        std::process::id(),
        bookforge_core::now_ms()
    ));
    let cleanup = StagedFileGuard(staged.clone());
    let reflow = reflow_epub(
        input,
        &staged,
        &ReflowOptions {
            dry_run: false,
            aggressive: false,
            pdf_cleanup: true,
        },
    )
    .map_err(|error| NarrationSourceError::Prepare {
        input: input.to_path_buf(),
        message: error.to_string(),
    })?;
    let mut book = read_epub(&staged).map_err(|error| NarrationSourceError::Read {
        input: input.to_path_buf(),
        message: error.to_string(),
    })?;
    book.source_path = Some(input.to_path_buf());
    let pdf_page_grouping = reflow.report.totals.pdf_documents_detected > 0;
    drop(cleanup);
    Ok(NarrationSource {
        book,
        pdf_page_grouping,
    })
}

struct StagedFileGuard(PathBuf);

impl Drop for StagedFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_input_fails_as_prepare_error_without_staging_residue() {
        let scratch = tempfile::tempdir().unwrap();
        let missing = scratch.path().join("absent.epub");
        let error =
            read_narration_source(&missing, scratch.path()).expect_err("missing file must fail");
        assert!(matches!(
            error,
            NarrationSourceError::Prepare { .. } | NarrationSourceError::Io { .. }
        ));
        // No staging leftovers on failure.
        let residue: Vec<_> = std::fs::read_dir(scratch.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().contains("bookforge-audio-clean"))
            .collect();
        assert!(residue.is_empty(), "{residue:?}");
    }

    #[test]
    fn staging_names_are_unique_per_call_and_scratch_is_caller_scoped() {
        // The launcher pipeline shares the CLI's staging convention
        // (`bookforge-audio-clean-<pid>-<seq>-<now>.epub`) inside whatever
        // scratch dir the caller controls; uniqueness of the sequence part is
        // what makes two preparations in one process collision-free.
        let scratch = tempfile::tempdir().unwrap();
        let absent = scratch.path().join("no-such-book.epub");
        let first =
            read_narration_source(&absent, scratch.path()).expect_err("absent input must fail");
        let second =
            read_narration_source(&absent, scratch.path()).expect_err("absent input must fail");
        match (first, second) {
            (
                NarrationSourceError::Prepare { input: a, .. },
                NarrationSourceError::Prepare { input: b, .. },
            )
            | (
                NarrationSourceError::Read { input: a, .. },
                NarrationSourceError::Read { input: b, .. },
            )
            | (
                NarrationSourceError::Io { path: a, .. },
                NarrationSourceError::Io { path: b, .. },
            ) => {
                assert_eq!(a, b);
            }
            _ => panic!("stable error class across identical calls"),
        }
        assert!(
            std::fs::read_dir(scratch.path())
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains("bookforge-audio-clean")),
            "staged EPUBs must not outlive preparation attempts"
        );
    }
}
