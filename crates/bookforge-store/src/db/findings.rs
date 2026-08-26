use super::*;

use std::collections::BTreeMap;

pub use bookforge_core::ir::QaFindingSeverity;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum QaFindingKind {
    ProtectedSpanMissing,
    InlineMarkerMissing,
    InlineMarkerDuplicated,
    InlineMarkerUnknown,
    MarkerStructure,
    BatchBlockMismatch,
    SourceCopyUnchanged,
    TargetLanguageGate,
    ProviderError,
    Interrupted,
    Other,
}

impl QaFindingKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProtectedSpanMissing => "protected_span_missing",
            Self::InlineMarkerMissing => "inline_marker_missing",
            Self::InlineMarkerDuplicated => "inline_marker_duplicated",
            Self::InlineMarkerUnknown => "inline_marker_unknown",
            Self::MarkerStructure => "marker_structure",
            Self::BatchBlockMismatch => "batch_block_mismatch",
            Self::SourceCopyUnchanged => "source_copy_unchanged",
            Self::TargetLanguageGate => "target_language_gate",
            Self::ProviderError => "provider_error",
            Self::Interrupted => "interrupted",
            Self::Other => "other",
        }
    }

    pub fn severity(self) -> QaFindingSeverity {
        match self {
            Self::ProtectedSpanMissing
            | Self::InlineMarkerMissing
            | Self::InlineMarkerDuplicated
            | Self::InlineMarkerUnknown
            | Self::MarkerStructure
            | Self::BatchBlockMismatch
            | Self::ProviderError
            | Self::Other => QaFindingSeverity::Error,
            Self::SourceCopyUnchanged | Self::TargetLanguageGate | Self::Interrupted => {
                QaFindingSeverity::Warning
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QaFinding {
    pub kind: QaFindingKind,
    pub severity: QaFindingSeverity,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredQaFinding {
    pub id: String,
    pub segment_id: String,
    pub kind: String,
    pub severity: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QaFindingCount {
    pub kind: String,
    pub severity: String,
    pub count: usize,
}

impl QaFindingCount {
    pub fn share_percent(&self, total: usize) -> f64 {
        if total == 0 {
            return 0.0;
        }
        (self.count as f64 * 1_000.0 / total as f64).round() / 10.0
    }
}

pub fn classify_segment_error(error: &str) -> Vec<QaFinding> {
    let error = error.trim();
    if error.is_empty() {
        return Vec::new();
    }

    let mut findings: Vec<QaFinding> = Vec::new();
    for encoded_fragment in error
        .split("; ")
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        let (explicit_severity, fragment) =
            if let Some(fragment) = encoded_fragment.strip_prefix("warning: ") {
                (Some(QaFindingSeverity::Warning), fragment)
            } else if let Some(fragment) = encoded_fragment.strip_prefix("error: ") {
                (Some(QaFindingSeverity::Error), fragment)
            } else {
                (None, encoded_fragment)
            };
        let kind = classify_failure(fragment);
        if kind == QaFindingKind::Other
            && let Some(previous) = findings.last_mut()
        {
            // Some real messages contain a literal "; " (for example the Toki
            // Pona gate's offending context). Merging unclassifiable tails back
            // keeps the row count equal to the number of real failures.
            previous.message.push_str("; ");
            previous.message.push_str(fragment);
            continue;
        }
        findings.push(QaFinding {
            kind,
            severity: explicit_severity.unwrap_or_else(|| kind.severity()),
            message: fragment.to_string(),
        });
    }
    findings
}

fn classify_failure(fragment: &str) -> QaFindingKind {
    let lower = fragment.to_lowercase();

    // First match wins. This order moves specific structural and validator
    // failures ahead of broad transport wording so wrapped errors retain their
    // most useful classification. In particular, Interrupted must precede
    // ProviderError because the real string is "provider error: interrupted by
    // user" after LlmError::Provider wraps the operator interrupt.
    if lower.contains("interrupted by user") {
        QaFindingKind::Interrupted
    } else if lower.contains("protected span missing") {
        QaFindingKind::ProtectedSpanMissing
    } else if lower.contains("inline marker missing") {
        QaFindingKind::InlineMarkerMissing
    } else if lower.contains("inline marker duplicated") {
        QaFindingKind::InlineMarkerDuplicated
    } else if lower.contains("unknown inline marker") {
        QaFindingKind::InlineMarkerUnknown
    } else if [
        "invalid inline marker structure",
        "unexpected inline marker close",
        "is closed by",
        "is missing closing tag",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        QaFindingKind::MarkerStructure
    } else if [
        "batch translation block mismatch",
        "missing block translations",
        "block translations, got",
        "returned duplicate block_id",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        QaFindingKind::BatchBlockMismatch
    } else if [
        "unchanged from the source-language prose",
        "of the source-language words",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        QaFindingKind::SourceCopyUnchanged
    } else if [
        "toki pona",
        "pi must group at least two following words",
        "en may only coordinate subjects",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        QaFindingKind::TargetLanguageGate
    } else if [
        "provider error",
        "http error",
        "http status",
        "rate limit",
        "unauthorized",
        "invalid api key",
        "connection",
        "timed out",
        "timeout",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        QaFindingKind::ProviderError
    } else {
        QaFindingKind::Other
    }
}

pub fn aggregate_findings(findings: impl IntoIterator<Item = QaFinding>) -> Vec<QaFindingCount> {
    let mut counts = BTreeMap::<(QaFindingKind, &'static str), usize>::new();
    for finding in findings {
        *counts
            .entry((finding.kind, finding.severity.as_str()))
            .or_default() += 1;
    }

    let mut breakdown = counts
        .into_iter()
        .map(|((kind, severity), count)| QaFindingCount {
            kind: kind.as_str().to_string(),
            severity: severity.to_string(),
            count,
        })
        .collect::<Vec<_>>();
    breakdown.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| left.kind.cmp(&right.kind))
    });
    breakdown
}

impl JobStore {
    /// Replace the stored findings for one segment with the classification of
    /// `error`. Returns how many rows were written.
    pub fn record_segment_findings(
        &self,
        job_id: &str,
        segment_id: &str,
        error: &str,
    ) -> Result<usize> {
        let mut conn = self.conn.borrow_mut();
        let tx = conn.transaction()?;
        let written = record_segment_findings_on(&tx, job_id, segment_id, error)?;
        tx.commit()?;
        Ok(written)
    }

    /// Replace the LLM-review findings for a job without disturbing findings
    /// produced by deterministic validators.
    ///
    /// `severity` is the model's `low`/`medium`/`high` value. High-severity
    /// findings become stored errors; every other value becomes a warning.
    /// Callers collapse repeated issues before this write and put the complete
    /// occurrence/segment/excerpt context in `message`.
    pub fn replace_llm_qa_findings(
        &self,
        job_id: &str,
        findings: &[(&str, &str, &str, &str)],
    ) -> Result<usize> {
        let mut conn = self.conn.borrow_mut();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM qa_findings WHERE job_id = ?1 AND kind GLOB 'llm_*'",
            params![job_id],
        )?;

        let mut inserted = 0usize;
        for (index, (segment_id, kind, severity, message)) in findings.iter().enumerate() {
            let exists = tx.query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM segments WHERE job_id = ?1 AND id = ?2
                 )",
                params![job_id, segment_id],
                |row| row.get::<_, bool>(0),
            )?;
            if !exists {
                continue;
            }

            let kind = llm_finding_kind(kind);
            let severity = llm_finding_severity(severity);
            let hash = stable_hash(&format!(
                "{job_id}\u{1f}llm\u{1f}{segment_id}\u{1f}{kind}\u{1f}{index}"
            ));
            let id = format!("qaf_{}", &hash[..24]);
            inserted += tx.execute(
                "INSERT INTO qa_findings
                 (id, segment_id, job_id, severity, kind, message)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![id, segment_id, job_id, severity, kind, message],
            )?;
        }
        tx.commit()?;
        Ok(inserted)
    }

    /// Drop deterministic findings recorded against one segment (used when a
    /// segment reaches a clean terminal state and `segments.error` goes back
    /// to NULL). LLM-review findings have their own replacement lifecycle.
    pub fn clear_segment_findings(&self, job_id: &str, segment_id: &str) -> Result<()> {
        let conn = self.conn.borrow();
        clear_segment_findings_on(&conn, job_id, segment_id)
    }

    /// Drop stale error findings for segments that no longer carry an error.
    /// Warnings may intentionally belong to succeeded segments.
    pub fn prune_stale_findings(&self, job_id: &str) -> Result<usize> {
        let conn = self.conn.borrow();
        Ok(conn.execute(
            "DELETE FROM qa_findings
             WHERE job_id = ?1
               AND severity = 'error'
               AND kind NOT GLOB 'llm_*'
               AND segment_id IN (
                 SELECT id FROM segments
                 WHERE job_id = ?1 AND (error IS NULL OR TRIM(error) = '')
               )",
            params![job_id],
        )?)
    }

    /// Every stored finding for a job, ordered by segment then id.
    pub fn segment_qa_findings(&self, job_id: &str) -> Result<Vec<StoredQaFinding>> {
        let conn = self.conn.borrow();
        let mut stmt = conn.prepare(
            "SELECT id, segment_id, kind, severity, message
             FROM qa_findings
             WHERE job_id = ?1
             ORDER BY segment_id, id",
        )?;
        let rows = stmt.query_map(params![job_id], |row| {
            Ok(StoredQaFinding {
                id: row.get(0)?,
                segment_id: row.get(1)?,
                kind: row.get(2)?,
                severity: row.get(3)?,
                message: row.get(4)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    /// Count of findings per (kind, severity), count-descending then
    /// kind-ascending.
    pub fn qa_finding_breakdown(&self, job_id: &str) -> Result<Vec<QaFindingCount>> {
        let conn = self.conn.borrow();
        let mut stmt = conn.prepare(
            "SELECT kind, severity, COUNT(*)
             FROM qa_findings
             WHERE job_id = ?1
             GROUP BY kind, severity
             ORDER BY COUNT(*) DESC, kind ASC",
        )?;
        let rows = stmt.query_map(params![job_id], |row| {
            Ok(QaFindingCount {
                kind: row.get(0)?,
                severity: row.get(1)?,
                count: row.get::<_, i64>(2)? as usize,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }
}

fn llm_finding_kind(kind: &str) -> String {
    let kind = kind.trim().trim_start_matches("llm_");
    if kind.is_empty() {
        "llm_other".to_string()
    } else {
        format!("llm_{kind}")
    }
}

fn llm_finding_severity(severity: &str) -> &'static str {
    if severity.trim().eq_ignore_ascii_case("high") {
        QaFindingSeverity::Error.as_str()
    } else {
        QaFindingSeverity::Warning.as_str()
    }
}

#[cfg(test)]
mod llm_findings_tests {
    use super::*;
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn llm_finding_round_trips_namespaced_with_mapped_severity() {
        let db_path = temp_db_path();
        let store = JobStore::open(&db_path).expect("store opens");
        {
            let conn = store.conn.borrow();
            conn.execute(
                "INSERT INTO jobs
                 (id, input_hash, target_lang, provider, model, status, created_at, updated_at)
                 VALUES ('job_qa', 'hash', 'Italian', 'mock', 'mock', 'running', 'now', 'now')",
                [],
            )
            .expect("job fixture inserts");
            conn.execute(
                "INSERT INTO segments
                 (id, job_id, section_id, ordinal, source_hash, prompt_version,
                  provider, model, status)
                 VALUES ('seg_0', 'job_qa', 'sec_0', 0, 'hash', 'v1',
                         'mock', 'mock', 'needs_review')",
                [],
            )
            .expect("segment fixture inserts");
        }

        store
            .record_segment_findings(
                "job_qa",
                "seg_0",
                "protected span missing from segment 'seg_0': The Cyberiad",
            )
            .expect("deterministic finding writes");
        store
            .replace_llm_qa_findings(
                "job_qa",
                &[(
                    "seg_0",
                    "mistranslation",
                    "high",
                    "high [mistranslation]: title changed occurrences=1 segments=[seg_0]",
                )],
            )
            .expect("LLM finding writes");
        store
            .record_segment_findings(
                "job_qa",
                "seg_0",
                "protected span missing from segment 'seg_0': The Cyberiad",
            )
            .expect("later deterministic refresh preserves LLM finding");

        let findings = store
            .segment_qa_findings("job_qa")
            .expect("findings round trip");
        assert_eq!(findings.len(), 2);
        let deterministic = findings
            .iter()
            .find(|finding| finding.kind == "protected_span_missing")
            .expect("deterministic finding remains distinguishable");
        assert_eq!(deterministic.severity, "error");
        let llm = findings
            .iter()
            .find(|finding| finding.kind == "llm_mistranslation")
            .expect("LLM finding has reserved namespace");
        assert_eq!(llm.severity, "error");
        assert!(llm.message.contains("segments=[seg_0]"));

        drop(store);
        let _ = fs::remove_file(db_path);
    }

    fn temp_db_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "bookforge-llm-findings-{}-{}-{}.sqlite",
            std::process::id(),
            unix_timestamp_nanos(),
            TEST_COUNTER.fetch_add(1, Ordering::Relaxed)
        ))
    }
}
