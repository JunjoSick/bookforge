//! Process exit-code taxonomy (UI-21 / DOC-3).
//!
//! BookForge commands use a small, documented set of exit codes:
//!
//! | Code | Meaning |
//! | ---- | ------- |
//! | 0    | Success, including an *intentional* stop (`stop`/`pause`, TUI quit after a normal finish). |
//! | 1    | Runtime failure: provider/config errors, IO errors, failed checks (`doctor`), incomplete deliverables. |
//! | 2    | Usage error from the argument parser (clap's standard code). |
//! | 3    | The job ran to its end but finished with unresolved segments (failed and/or needs-review); the output EPUB is written but not clean. |
//! | 130  | Interrupted by the user (Ctrl+C / TUI cancel) after a graceful checkpoint; progress is saved for `resume`. |
//!
//! Commands request a specific code via [`request`] right before returning
//! `Ok(())`; [`resolve`] then combines that request with whether the command
//! ended in an error. A user interruption always wins (the shell convention
//! treats 128+SIGINT as "killed by Ctrl+C" regardless of any later error),
//! then runtime failures (1), then whatever was requested.

use std::sync::atomic::{AtomicI32, Ordering};

pub(crate) const SUCCESS: i32 = 0;
pub(crate) const FAILURE: i32 = 1;
/// The run completed but some segments remain failed/needs-review.
pub(crate) const COMPLETED_WITH_FAILURES: i32 = 3;
/// Graceful SIGINT/Ctrl+C interruption (128 + SIGINT), matching POSIX shells.
pub(crate) const INTERRUPTED: i32 = 130;

static REQUESTED: AtomicI32 = AtomicI32::new(SUCCESS);

/// Record that this command wants to exit with `code` despite returning `Ok`.
///
/// Later calls overwrite earlier ones; precedence between a requested code and
/// a returned error is decided once in [`resolve`].
pub(crate) fn request(code: i32) {
    REQUESTED.store(code, Ordering::SeqCst);
}

fn requested() -> i32 {
    REQUESTED.load(Ordering::SeqCst)
}

/// Combine the command result with any explicit request into a final code.
pub(crate) fn resolve(run_failed: bool) -> i32 {
    let requested = requested();
    if requested == INTERRUPTED {
        return INTERRUPTED;
    }
    if run_failed {
        return FAILURE;
    }
    requested
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reset() {
        REQUESTED.store(SUCCESS, Ordering::SeqCst);
    }

    #[test]
    fn default_is_success() {
        reset();
        assert_eq!(resolve(false), SUCCESS);
    }

    #[test]
    fn runtime_errors_exit_one_even_with_an_earlier_request() {
        reset();
        request(COMPLETED_WITH_FAILURES);
        assert_eq!(resolve(true), FAILURE);
    }

    #[test]
    fn completed_with_unresolved_segments_reports_three() {
        reset();
        request(COMPLETED_WITH_FAILURES);
        assert_eq!(resolve(false), COMPLETED_WITH_FAILURES);
    }

    #[test]
    fn interruption_beats_everything() {
        reset();
        request(INTERRUPTED);
        assert_eq!(resolve(false), INTERRUPTED);
        assert_eq!(resolve(true), INTERRUPTED);
    }

    #[test]
    fn taxonomy_values_are_the_documented_constants() {
        assert_eq!(SUCCESS, 0);
        assert_eq!(FAILURE, 1);
        assert_eq!(COMPLETED_WITH_FAILURES, 3);
        // POSIX convention: 128 + SIGINT(2).
        assert_eq!(INTERRUPTED, 130);
    }
}
