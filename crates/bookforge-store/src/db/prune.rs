//! Retention/prune path for store growth (STORE-17 part A).
//!
//! Deleting a job means deleting its whole dependency tree: `qa_findings`,
//! `translation_blocks`, `translations`, `segments`, `segment_flags`, and
//! finally the `jobs` row itself. Every job is deleted in ONE `IMMEDIATE`
//! transaction so a crash never strands half a tree, and jobs whose status is
//! `running` are always protected — they may be actively checkpointing from
//! another process.

use super::*;
use rusqlite::TransactionBehavior;
use std::time::{SystemTime, UNIX_EPOCH};

/// Selection options for [`JobStore::prune_jobs`]. All fields compose:
/// `older_than` filters by creation age first, then `keep_last_n` retains the
/// N most recent surviving candidates from deletion.
#[derive(Debug, Clone, Copy, Default)]
pub struct PruneJobsOptions {
    /// Only consider jobs created strictly before this instant. `None` means
    /// no age floor.
    pub older_than: Option<SystemTime>,
    /// Always retain the newest N eligible candidates, regardless of age.
    pub keep_last_n: Option<usize>,
    /// When true nothing is modified; the report describes exactly what would
    /// happen under identical selection rules.
    pub dry_run: bool,
}

/// Per-job deletion breakdown (or would-be deletion under dry-run).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PruneJobDeletion {
    pub job_id: String,
    pub segments: usize,
    pub translations: usize,
    pub translation_blocks: usize,
    pub qa_findings: usize,
    pub segment_flags: usize,
    /// Artifact files successfully unlinked: events log, JSON report,
    /// markdown report.
    pub artifacts_removed: Vec<PathBuf>,
    /// Artifact paths that were recorded on the job but did not exist.
    pub artifacts_missing: usize,
}

impl PruneJobDeletion {
    /// Rows removed across all job-owned child tables.
    pub fn total_rows(&self) -> usize {
        self.segments
            + self.translations
            + self.translation_blocks
            + self.qa_findings
            + self.segment_flags
    }
}

/// Aggregate outcome of one [`JobStore::prune_jobs`] call.
#[derive(Debug, Clone, Default)]
pub struct PruneJobsReport {
    /// Mirrors [`PruneJobsOptions::dry_run`] so callers can tell whether any
    /// of this actually happened.
    pub dry_run: bool,
    /// Non-running jobs matched by the age filter (before keep_last_n).
    pub candidate_count: usize,
    /// Jobs withheld by `keep_last_n`.
    pub retained_by_keep_last_n: usize,
    /// Running jobs found in the store; always protected, never counted as
    /// candidates.
    pub protected_running_jobs: usize,
    pub deletions: Vec<PruneJobDeletion>,
}

impl PruneJobsReport {
    pub fn pruned_job_count(&self) -> usize {
        self.deletions.len()
    }

    pub fn total_rows_deleted(&self) -> usize {
        self.deletions
            .iter()
            .map(PruneJobDeletion::total_rows)
            .sum()
    }
}

fn epoch_seconds(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs())
}

impl JobStore {
    /// Delete finished jobs and every row/file they own per the given filter.
    ///
    /// Rules:
    /// - Jobs with status `running` are NEVER touched (`PruneJobsReport::
    ///   protected_running_jobs` reports how many were skipped for this
    ///   reason).
    /// - Selection order: `older_than` cutoff on `created_at`, newest-first;
    ///   `keep_last_n` spares the N newest survivors; both compose.
    /// - DB rows go atomically per job inside an IMMEDIATE transaction, in
    ///   child-before-parent order so FK violations can never abort mid-tree.
    /// - Artifact files (events/report json/markdown) recorded on the job are
    ///   unlinked right after that transaction commits. Missing files count
    ///   as `artifacts_missing` and other unlink failures are ignored: rows
    ///   are already gone, so files cannot be resurrected into summaries.
    ///   The input snapshot is intentionally kept — it documents what the
    ///   original input was.
    pub fn prune_jobs(&self, options: PruneJobsOptions) -> Result<PruneJobsReport> {
        let cutoff_secs = options
            .older_than
            .and_then(epoch_seconds)
            .and_then(|secs| i64::try_from(secs).ok());
        let running_guard = JobStatus::Running.as_db_text();

        let mut report = PruneJobsReport {
            dry_run: options.dry_run,
            ..PruneJobsReport::default()
        };

        // Single consistent snapshot for selection: read candidates and the
        // running-job guard count together before deleting anything.
        let mut conn = self.conn.borrow_mut();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let select_sql = "SELECT id FROM jobs
             WHERE status <> ?1
               AND (?2 IS NULL OR CAST(created_at AS INTEGER) < ?2)
             ORDER BY CAST(created_at AS INTEGER) DESC, rowid DESC";
        let mut stmt = tx.prepare(select_sql)?;
        let candidate_ids = stmt
            .query_map(params![running_guard, cutoff_secs], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);

        report.protected_running_jobs = tx.query_row(
            "SELECT COUNT(*) FROM jobs WHERE status = ?1",
            params![running_guard],
            |row| row.get::<_, i64>(0),
        )? as usize;
        report.candidate_count = candidate_ids.len();
        let to_delete: &[String] = match options.keep_last_n {
            Some(keep) => &candidate_ids[keep.min(candidate_ids.len())..],
            None => &candidate_ids,
        };
        report.retained_by_keep_last_n = candidate_ids.len() - to_delete.len();
        tx.commit()?;
        // Release the connection before per-job operations re-borrow it.
        drop(conn);

        for job_id in to_delete {
            let deletion = if options.dry_run {
                self.prune_job_dry_run(job_id)?
            } else {
                self.prune_job_now(job_id)?
            };
            report.deletions.push(deletion);
        }
        Ok(report)
    }

    fn prune_job_deletion_columns(conn: &Connection, job_id: &str) -> Result<PruneJobDeletion> {
        let count = |table: &str| -> Result<usize> {
            Ok(conn.query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE job_id = ?1"),
                params![job_id],
                |row| row.get::<_, i64>(0),
            )? as usize)
        };
        Ok(PruneJobDeletion {
            job_id: job_id.to_string(),
            segments: count("segments")?,
            translations: count("translations")?,
            translation_blocks: count("translation_blocks")?,
            qa_findings: count("qa_findings")?,
            segment_flags: count("segment_flags")?,
            artifacts_removed: Vec::new(),
            artifacts_missing: 0,
        })
    }

    fn prune_job_dry_run(&self, job_id: &str) -> Result<PruneJobDeletion> {
        let conn = self.conn.borrow();
        let mut deletion = Self::prune_job_deletion_columns(&conn, job_id)?;
        deletion.artifacts_missing = artifact_paths(&conn, job_id)?
            .into_iter()
            .filter(|path| !path.exists())
            .count();
        Ok(deletion)
    }

    fn prune_job_now(&self, job_id: &str) -> Result<PruneJobDeletion> {
        // Child-before-parent inside one IMMEDIATE transaction; only plain FKs
        // point at segments/translations/blocks/findings, so explicit deletes
        // keep this correct even where ON DELETE CASCADE is absent.
        let mut conn = self.conn.borrow_mut();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let qa_findings = delete_job_rows(&tx, "qa_findings", job_id)?;
        let translation_blocks = delete_job_rows(&tx, "translation_blocks", job_id)?;
        let translations = delete_job_rows(&tx, "translations", job_id)?;
        let segments = delete_job_rows(&tx, "segments", job_id)?;
        let segment_flags = delete_job_rows(&tx, "segment_flags", job_id)?;
        // Read referenced artifacts while the job row still exists.
        let artifacts = artifact_paths(&tx, job_id)?;
        let removed_jobs = tx.execute("DELETE FROM jobs WHERE id = ?1", params![job_id])?;
        tx.commit()?;

        if removed_jobs == 0 {
            // Raced with another process pruning the same job: its tree is
            // already gone, report a no-op.
            return Ok(PruneJobDeletion {
                job_id: job_id.to_string(),
                ..PruneJobDeletion::default()
            });
        }

        let mut missing = 0usize;
        let mut removed_files = Vec::new();
        for path in artifacts {
            match fs::remove_file(&path) {
                Ok(()) => removed_files.push(path),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => missing += 1,
                Err(_) => {}
            }
        }
        Ok(PruneJobDeletion {
            job_id: job_id.to_string(),
            segments,
            translations,
            translation_blocks,
            qa_findings,
            segment_flags,
            artifacts_removed: removed_files,
            artifacts_missing: missing,
        })
    }
}

/// Delete all rows of `table` scoped to one job.
fn delete_job_rows(tx: &rusqlite::Transaction<'_>, table: &str, job_id: &str) -> Result<usize> {
    Ok(tx.execute(
        &format!("DELETE FROM {table} WHERE job_id = ?1"),
        params![job_id],
    )?)
}

/// Artifact files referenced by the job record itself: events log plus the
/// JSON/markdown reports. Best-effort cleanup targets only.
fn artifact_paths(conn: &Connection, job_id: &str) -> Result<Vec<PathBuf>> {
    let row = conn
        .query_row(
            "SELECT events_path, report_json_path, report_markdown_path
             FROM jobs WHERE id = ?1",
            params![job_id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((events, report_json, report_markdown)) = row else {
        return Ok(Vec::new());
    };
    Ok([events, report_json, report_markdown]
        .into_iter()
        .flatten()
        .filter(|text| !text.trim().is_empty())
        .map(PathBuf::from)
        .collect())
}
