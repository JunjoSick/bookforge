//! Typed lifecycle statuses at the store boundary (STORE-12).
//!
//! The database keeps `TEXT` columns unchanged — external serialized formats
//! never changed — but every status handed in or out of [`crate::db`] types is
//! represented by these enums. Values written by BookForge always come from
//! the known variant sets (now additionally enforced by CHECK constraints in
//! the storage layer). Pre-existing databases could contain anything history
//! wrote or hand-edits introduced, so decoding is defensive: unrecognized text
//! decodes to `Unknown(<verbatim>)` instead of panicking, preserving the exact
//! bytes so reads and diagnostic round-trips never lose data.

/// Lifecycle state of a `jobs` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobStatus {
    /// Worker is actively translating segments.
    Running,
    /// Operator paused; safe boundaries do no further work.
    Paused,
    /// Operator stopped the job; resume may continue later.
    Stopped,
    /// Process ended without reaching a terminal state (crash/Ctrl+C).
    Interrupted,
    /// Every segment reached a resolved state.
    Succeeded,
    /// Job gave up (retry budget exhausted / fatal provider failure).
    Failed,
    /// At least one segment awaits human review.
    NeedsReview,
    /// Failures were reset for another pass.
    RetryPending,
    /// Text not produced by BookForge (e.g. hand-edited database row).
    /// Preserved verbatim for read transparency; writes reject such values.
    Unknown(String),
}

impl JobStatus {
    /// Every status BookForge itself writes, in canonical order.
    pub const KNOWN_DB_TEXTS: &'static [&'static str] = &[
        "running",
        "paused",
        "stopped",
        "interrupted",
        "succeeded",
        "failed",
        "needs_review",
        "retry_pending",
    ];

    pub fn from_db_text(text: &str) -> Self {
        match text {
            "running" => Self::Running,
            "paused" => Self::Paused,
            "stopped" => Self::Stopped,
            "interrupted" => Self::Interrupted,
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            "needs_review" => Self::NeedsReview,
            "retry_pending" => Self::RetryPending,
            other => Self::Unknown(other.to_string()),
        }
    }

    /// Storage text: canonical for known variants, verbatim for [`Self::Unknown`]
    /// (round-trips hand-edited rows without data loss).
    pub fn as_db_text(&self) -> &str {
        match self {
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Stopped => "stopped",
            Self::Interrupted => "interrupted",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::NeedsReview => "needs_review",
            Self::RetryPending => "retry_pending",
            Self::Unknown(text) => text,
        }
    }

    /// Human-facing label; identical to the stored text for compatibility with
    /// dashboards and reports that render the historical strings.
    pub fn label(&self) -> &str {
        self.as_db_text()
    }

    /// All canonical variants as typed values (same order as
    /// [`Self::KNOWN_DB_TEXTS`]).
    pub fn all_known() -> &'static [Self] {
        &[
            Self::Running,
            Self::Paused,
            Self::Stopped,
            Self::Interrupted,
            Self::Succeeded,
            Self::Failed,
            Self::NeedsReview,
            Self::RetryPending,
        ]
    }
}

impl std::fmt::Display for JobStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Lifecycle state of a `segments` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SegmentStatus {
    /// Scheduled, not yet attempted by a worker.
    Queued,
    /// Translated successfully.
    Succeeded,
    /// Latest attempt errored; eligible for retry.
    Failed,
    /// Reset by operator/provider guidance; awaiting the next pass.
    RetryPending,
    /// Needs human review before becoming authoritative.
    NeedsReview,
    /// Fulfilled entirely from the cache namespace lookup.
    SkippedCached,
    /// Text not produced by BookForge (e.g. hand-edited database row).
    /// Preserved verbatim for read transparency; writes reject such values.
    Unknown(String),
}

impl SegmentStatus {
    /// Every status BookForge itself writes, in canonical order.
    pub const KNOWN_DB_TEXTS: &'static [&'static str] = &[
        "queued",
        "succeeded",
        "failed",
        "retry_pending",
        "needs_review",
        "skipped_cached",
    ];

    pub fn from_db_text(text: &str) -> Self {
        match text {
            "queued" => Self::Queued,
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            "retry_pending" => Self::RetryPending,
            "needs_review" => Self::NeedsReview,
            "skipped_cached" => Self::SkippedCached,
            other => Self::Unknown(other.to_string()),
        }
    }

    /// Storage text: canonical for known variants, verbatim for [`Self::Unknown`]
    /// (round-trips hand-edited rows without data loss).
    pub fn as_db_text(&self) -> &str {
        match self {
            Self::Queued => "queued",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::RetryPending => "retry_pending",
            Self::NeedsReview => "needs_review",
            Self::SkippedCached => "skipped_cached",
            Self::Unknown(text) => text,
        }
    }

    /// Statuses a worker treats as already-finished work.
    pub fn resolved() -> &'static [Self] {
        static RESOLVED: [SegmentStatus; 2] =
            [SegmentStatus::Succeeded, SegmentStatus::SkippedCached];
        &RESOLVED
    }

    /// Statuses that carry an authoritative translation row and must never be
    /// clobbered by a failure marking.
    pub fn terminal_with_translation() -> &'static [Self] {
        static TERMINAL: [SegmentStatus; 3] = [
            SegmentStatus::Succeeded,
            SegmentStatus::SkippedCached,
            SegmentStatus::NeedsReview,
        ];
        &TERMINAL
    }

    /// Statuses still owed pipeline work by a resuming scheduler.
    pub fn resumable() -> &'static [Self] {
        static RESUMABLE: [SegmentStatus; 3] = [
            SegmentStatus::Queued,
            SegmentStatus::RetryPending,
            SegmentStatus::Failed,
        ];
        &RESUMABLE
    }

    /// All canonical variants as typed values (same order as
    /// [`Self::KNOWN_DB_TEXTS`]).
    pub fn all_known() -> &'static [Self] {
        &[
            Self::Queued,
            Self::Succeeded,
            Self::Failed,
            Self::RetryPending,
            Self::NeedsReview,
            Self::SkippedCached,
        ]
    }

    /// Statuses grouped into one SQL membership list. Values are constant
    /// identifiers emitted by [`Self::as_db_text`]; nothing injectable flows
    /// through here.
    pub fn sql_set(values: &[Self]) -> String {
        let mut out = String::with_capacity(values.len() * 18);
        for (index, value) in values.iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            out.push('\'');
            out.push_str(value.as_db_text());
            out.push('\'');
        }
        out
    }
}

impl std::fmt::Display for SegmentStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_db_text())
    }
}

/// Quoted `, `-joined SQL list for job statuses (same guarantees as
/// [`SegmentStatus::sql_set`]).
pub(super) fn job_sql_set(values: &[JobStatus]) -> String {
    let mut out = String::with_capacity(values.len() * 16);
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        out.push('\'');
        out.push_str(value.as_db_text());
        out.push('\'');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_statuses_round_trip_every_known_text() {
        for text in JobStatus::KNOWN_DB_TEXTS {
            let decoded = JobStatus::from_db_text(text);
            assert_eq!(decoded.as_db_text(), *text);
            assert_ne!(decoded, JobStatus::Unknown((*text).to_string()));
            assert_eq!(decoded.to_string(), *text);
        }
    }

    #[test]
    fn segment_statuses_round_trip_every_known_text() {
        for text in SegmentStatus::KNOWN_DB_TEXTS {
            let decoded = SegmentStatus::from_db_text(text);
            assert_eq!(decoded.as_db_text(), *text);
            assert_ne!(decoded, SegmentStatus::Unknown((*text).to_string()));
            assert_eq!(decoded.to_string(), *text);
        }
    }

    #[test]
    fn unknown_values_decode_defensively_and_round_trip_verbatim() {
        let mystery = "weird vendor patch";
        let decoded = JobStatus::from_db_text(mystery);
        assert_eq!(decoded, JobStatus::Unknown(mystery.to_string()));
        assert_eq!(decoded.as_db_text(), mystery, "unknown text is preserved");

        let segment_mystery = SegmentStatus::from_db_text("");
        assert_eq!(segment_mystery, SegmentStatus::Unknown(String::new()));
        assert_eq!(segment_mystery.as_db_text(), "");
    }

    #[test]
    fn sql_sets_render_quoted_canonical_literals() {
        // Callers wrap these in parentheses themselves.
        assert_eq!(
            SegmentStatus::sql_set(SegmentStatus::resolved()),
            "'succeeded', 'skipped_cached'"
        );
        assert_eq!(job_sql_set(&[JobStatus::Running]), "'running'");
        assert!(SegmentStatus::sql_set(&[]).is_empty());
        assert_eq!(
            job_sql_set(JobStatus::all_known()),
            "'running', 'paused', 'stopped', 'interrupted', \
             'succeeded', 'failed', 'needs_review', 'retry_pending'"
        );
    }
}
