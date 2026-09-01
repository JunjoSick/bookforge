use super::*;

impl JobStore {
    pub fn create_job(&self, request: CreateJob<'_>) -> Result<JobRecord> {
        let input_hash = file_hash(request.input)?;
        let id = format!("job_{}_{}", unix_timestamp_nanos(), &input_hash[..12]);
        let now = timestamp_string();
        let input_path = request.input.to_path_buf();
        let output_path = request.output.to_path_buf();
        let conn = self.conn.borrow();
        let sql = format!(
            "INSERT INTO jobs
             (id, input_path, output_path, input_hash, source_lang, target_lang, provider, model, base_url, api_key_env, book_id, series_id, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, '{}', ?13, ?13)",
            JobStatus::Running.as_db_text()
        );
        conn.execute(
            &sql,
            params![
                id,
                input_path.to_string_lossy(),
                output_path.to_string_lossy(),
                input_hash,
                request.source_lang,
                request.target_lang,
                request.provider,
                request.model,
                request.base_url,
                request.api_key_env,
                request.book_id,
                request.series_id,
                now,
            ],
        )?;

        Ok(JobRecord {
            id,
            input_path,
            input_snapshot_path: None,
            input_sha256: None,
            output_path,
            input_hash,
            source_lang: request.source_lang.map(ToOwned::to_owned),
            target_lang: request.target_lang.to_string(),
            provider: request.provider.to_string(),
            model: request.model.to_string(),
            base_url: request.base_url.map(ToOwned::to_owned),
            api_key_env: request.api_key_env.map(ToOwned::to_owned),
            status: JobStatus::Running.label().to_string(),
            events_path: None,
            report_json_path: None,
            report_markdown_path: None,
            book_id: request.book_id.map(ToOwned::to_owned),
            series_id: request.series_id.map(ToOwned::to_owned),
        })
    }

    pub fn update_job_config_snapshot(
        &self,
        job_id: &str,
        snapshot: &RunConfigSnapshot,
    ) -> Result<()> {
        let json = serde_json::to_string(snapshot)
            .map_err(|e| StoreError::Serialization(e.to_string()))?;
        let conn = self.conn.borrow();
        ensure_job_exists(&conn, job_id)?;
        conn.execute(
            "UPDATE jobs
             SET config_json = ?1,
                 events_path = ?2,
                 report_json_path = ?3,
                 report_markdown_path = ?4,
                 input_snapshot_path = ?5,
                 input_sha256 = ?6,
                 updated_at = ?7
             WHERE id = ?8",
            params![
                json,
                snapshot
                    .events_path
                    .as_ref()
                    .map(|path| path.to_string_lossy().to_string()),
                snapshot
                    .report_json_path
                    .as_ref()
                    .map(|path| path.to_string_lossy().to_string()),
                snapshot
                    .report_markdown_path
                    .as_ref()
                    .map(|path| path.to_string_lossy().to_string()),
                snapshot
                    .input_snapshot_path
                    .as_ref()
                    .map(|path| path.to_string_lossy().to_string()),
                snapshot.input_sha256.as_deref(),
                timestamp_string(),
                job_id,
            ],
        )?;
        Ok(())
    }

    pub fn update_job_input_snapshot(
        &self,
        job_id: &str,
        snapshot_path: &Path,
        input_sha256: &str,
    ) -> Result<()> {
        let conn = self.conn.borrow();
        ensure_job_exists(&conn, job_id)?;
        conn.execute(
            "UPDATE jobs
             SET input_snapshot_path = ?1,
                 input_sha256 = ?2,
                 updated_at = ?3
             WHERE id = ?4",
            params![
                snapshot_path.to_string_lossy(),
                input_sha256,
                timestamp_string(),
                job_id
            ],
        )?;
        Ok(())
    }

    pub fn load_job_config_snapshot(&self, job_id: &str) -> Result<Option<RunConfigSnapshot>> {
        let conn = self.conn.borrow();
        let Some(json) = conn
            .query_row(
                "SELECT config_json FROM jobs WHERE id = ?1",
                params![job_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten()
        else {
            return Ok(None);
        };

        serde_json::from_str(&json)
            .map(Some)
            .map_err(|e| StoreError::Serialization(e.to_string()))
    }

    pub fn update_job_event_path(&self, job_id: &str, path: &Path) -> Result<()> {
        let conn = self.conn.borrow();
        ensure_job_exists(&conn, job_id)?;
        conn.execute(
            "UPDATE jobs SET events_path = ?1, updated_at = ?2 WHERE id = ?3",
            params![path.to_string_lossy(), timestamp_string(), job_id],
        )?;
        Ok(())
    }

    pub fn update_job_report_paths(
        &self,
        job_id: &str,
        json_path: &Path,
        markdown_path: &Path,
    ) -> Result<()> {
        let conn = self.conn.borrow();
        ensure_job_exists(&conn, job_id)?;
        conn.execute(
            "UPDATE jobs
             SET report_json_path = ?1, report_markdown_path = ?2, updated_at = ?3
             WHERE id = ?4",
            params![
                json_path.to_string_lossy(),
                markdown_path.to_string_lossy(),
                timestamp_string(),
                job_id
            ],
        )?;
        Ok(())
    }

    pub fn update_job_output_path(&self, job_id: &str, path: &Path) -> Result<()> {
        let conn = self.conn.borrow();
        ensure_job_exists(&conn, job_id)?;
        conn.execute(
            "UPDATE jobs SET output_path = ?1, updated_at = ?2 WHERE id = ?3",
            params![path.to_string_lossy(), timestamp_string(), job_id],
        )?;
        Ok(())
    }

    pub fn recompute_job_status(&self, job_id: &str) -> Result<()> {
        let resolved = SegmentStatus::sql_set(SegmentStatus::resolved());
        let conn = self.conn.borrow();
        let sql = format!(
            "SELECT COUNT(*),
                    COALESCE(SUM(CASE WHEN status IN ({resolved}) THEN 0 ELSE 1 END), 0)
             FROM segments WHERE job_id = ?1"
        );
        let (total, unresolved) = conn.query_row(&sql, params![job_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })?;
        drop(conn);
        if total > 0 && unresolved == 0 {
            self.touch_job(job_id, JobStatus::Succeeded)
        } else {
            self.touch_job(job_id, JobStatus::NeedsReview)
        }
    }

    pub fn mark_job_complete(&self, job_id: &str) -> Result<()> {
        self.touch_job_unless_status(job_id, JobStatus::Succeeded, &[JobStatus::Stopped])
    }

    pub fn mark_job_running(&self, job_id: &str) -> Result<()> {
        self.touch_job_unless_status(job_id, JobStatus::Running, &[JobStatus::Stopped])
    }

    pub fn mark_job_running_for_resume(&self, job_id: &str) -> Result<()> {
        self.touch_job(job_id, JobStatus::Running)
    }

    pub fn mark_job_paused(&self, job_id: &str) -> Result<()> {
        self.touch_job_unless_status(job_id, JobStatus::Paused, &[JobStatus::Stopped])
    }

    pub fn mark_job_stopped(&self, job_id: &str) -> Result<()> {
        self.touch_job(job_id, JobStatus::Stopped)
    }

    pub fn mark_job_succeeded(&self, job_id: &str) -> Result<()> {
        self.mark_job_complete(job_id)
    }

    pub fn mark_job_needs_review(&self, job_id: &str) -> Result<()> {
        self.touch_job_unless_status(job_id, JobStatus::NeedsReview, &[JobStatus::Stopped])
    }

    pub fn mark_job_interrupted(&self, job_id: &str) -> Result<()> {
        self.touch_job_unless_status(job_id, JobStatus::Interrupted, &[JobStatus::Stopped])
    }

    pub fn mark_job_failed(&self, job_id: &str) -> Result<()> {
        self.touch_job_unless_status(job_id, JobStatus::Failed, &[JobStatus::Stopped])
    }

    /// Force a segment into `failed` regardless of its current status.
    ///
    /// Production checkpoint paths should prefer
    /// [`JobStore::mark_segment_failed_if_unfinished`], which refuses to
    /// clobber terminal-with-translation states (`succeeded`,
    /// `needs_review`, ...) so a late failure report can never destroy work
    /// already persisted. Keep this twin only where overwriting is the point
    /// (fixtures, deliberate state repair).
    pub fn mark_segment_failed(&self, job_id: &str, segment_id: &str, error: &str) -> Result<()> {
        let updated = {
            let conn = self.conn.borrow();
            ensure_job_exists(&conn, job_id)?;
            let sql = format!(
                "UPDATE segments SET status = '{}', attempts = attempts + 1, error = ?1
                 WHERE job_id = ?2 AND id = ?3",
                SegmentStatus::Failed.as_db_text()
            );
            conn.execute(&sql, params![error, job_id, segment_id])?
        };
        if updated == 0 {
            return Err(StoreError::NotFound(format!(
                "segment '{segment_id}' was not found in job '{job_id}'"
            )));
        }
        // Findings are instrumentation, so a failed findings write must never
        // fail the surrounding translation checkpoint.
        let _ = self.record_segment_findings(job_id, segment_id, error);
        self.mark_job_failed(job_id)?;
        Ok(())
    }

    pub fn mark_segment_failed_if_unfinished(
        &self,
        job_id: &str,
        segment_id: &str,
        error: &str,
    ) -> Result<()> {
        let updated = {
            let conn = self.conn.borrow();
            ensure_job_exists(&conn, job_id)?;
            let sql = format!(
                "UPDATE segments
                 SET status = '{}', attempts = attempts + 1, error = ?1
                 WHERE job_id = ?2
                   AND id = ?3
                   AND status NOT IN ({})",
                SegmentStatus::Failed.as_db_text(),
                SegmentStatus::sql_set(SegmentStatus::terminal_with_translation())
            );
            conn.execute(&sql, params![error, job_id, segment_id])?
        };
        if updated == 0 && !segment_exists(&self.conn.borrow(), job_id, segment_id)? {
            // Zero rows can legitimately mean "the segment already reached a
            // terminal-with-translation state" (an intentional no-op); a
            // segment that does not EXIST at all must surface as NotFound.
            return Err(StoreError::NotFound(format!(
                "segment '{segment_id}' was not found in job '{job_id}'"
            )));
        }
        if updated > 0 {
            // Findings are instrumentation, so a failed findings write must
            // never fail the surrounding translation checkpoint.
            let _ = self.record_segment_findings(job_id, segment_id, error);
        }
        self.mark_job_failed(job_id)?;
        Ok(())
    }

    pub fn mark_unfinished_segments_failed(
        &self,
        job_id: &str,
        candidate_segment_ids: &[String],
        error: &str,
    ) -> Result<usize> {
        ensure_job_exists(&self.conn.borrow(), job_id)?;
        const SQLITE_IN_CHUNK_SIZE: usize = 900;
        let mut updated = 0;
        let failed_status = SegmentStatus::Failed.as_db_text();
        let untouched = SegmentStatus::sql_set(SegmentStatus::terminal_with_translation());

        for chunk in candidate_segment_ids.chunks(SQLITE_IN_CHUNK_SIZE) {
            if chunk.is_empty() {
                continue;
            }

            let placeholders = std::iter::repeat_n("?", chunk.len())
                .collect::<Vec<_>>()
                .join(", ");
            let select_sql = format!(
                "SELECT id FROM segments
                 WHERE job_id = ?
                   AND id IN ({placeholders})
                   AND status NOT IN ({untouched})
                 ORDER BY id"
            );
            let update_sql = format!(
                "UPDATE segments
                 SET status = '{failed_status}', attempts = attempts + 1, error = ?
                 WHERE job_id = ?
                   AND id IN ({placeholders})
                   AND status NOT IN ({untouched})"
            );

            let (chunk_updated, failed_segment_ids) = {
                let conn = self.conn.borrow();
                let mut select_params: Vec<&dyn rusqlite::types::ToSql> =
                    Vec::with_capacity(chunk.len() + 1);
                select_params.push(&job_id);
                for id in chunk {
                    select_params.push(id);
                }
                let mut stmt = conn.prepare(&select_sql)?;
                let failed_segment_ids = stmt
                    .query_map(select_params.as_slice(), |row| row.get::<_, String>(0))?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                drop(stmt);

                let mut update_params: Vec<&dyn rusqlite::types::ToSql> =
                    Vec::with_capacity(chunk.len() + 2);
                update_params.push(&error);
                update_params.push(&job_id);
                for id in chunk {
                    update_params.push(id);
                }
                let chunk_updated = conn.execute(&update_sql, update_params.as_slice())?;
                (chunk_updated, failed_segment_ids)
            };
            updated += chunk_updated;
            for segment_id in failed_segment_ids {
                // Findings are instrumentation, so a failed findings write
                // must never fail the surrounding translation checkpoint.
                let _ = self.record_segment_findings(job_id, &segment_id, error);
            }
        }

        if updated > 0 {
            self.mark_job_failed(job_id)?;
        }
        Ok(updated)
    }

    /// Column list for `jobs` in the exact order [`job_record_from_row`] reads.
    /// Shared by `get_job` and `list_job_summaries` so the SELECT and the mapper
    /// never drift.
    const JOB_COLUMNS: &'static str = "id, input_path, input_snapshot_path, input_sha256, output_path, input_hash, source_lang, target_lang, provider, model, base_url, api_key_env, status, events_path, report_json_path, report_markdown_path, book_id, series_id";

    fn job_record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<JobRecord> {
        Ok(JobRecord {
            id: row.get(0)?,
            input_path: PathBuf::from(row.get::<_, String>(1)?),
            input_snapshot_path: row.get::<_, Option<String>>(2)?.map(PathBuf::from),
            input_sha256: row.get(3)?,
            output_path: PathBuf::from(row.get::<_, String>(4)?),
            input_hash: row.get(5)?,
            source_lang: row.get(6)?,
            target_lang: row.get(7)?,
            provider: row.get(8)?,
            model: row.get(9)?,
            base_url: row.get(10)?,
            api_key_env: row.get(11)?,
            status: row.get(12)?,
            events_path: row.get::<_, Option<String>>(13)?.map(PathBuf::from),
            report_json_path: row.get::<_, Option<String>>(14)?.map(PathBuf::from),
            report_markdown_path: row.get::<_, Option<String>>(15)?.map(PathBuf::from),
            book_id: row.get(16)?,
            series_id: row.get(17)?,
        })
    }

    pub fn get_job(&self, job_id: &str) -> Result<Option<JobRecord>> {
        let conn = self.conn.borrow();
        conn.query_row(
            &format!("SELECT {} FROM jobs WHERE id = ?1", Self::JOB_COLUMNS),
            params![job_id],
            Self::job_record_from_row,
        )
        .optional()
        .map_err(StoreError::from)
    }

    pub fn summary(&self, job_id: &str) -> Result<Option<JobSummary>> {
        let Some(job) = self.get_job(job_id)? else {
            return Ok(None);
        };
        let conn = self.conn.borrow();
        let mut summary = JobSummary {
            id: job.id,
            status: job.status,
            ..JobSummary::default()
        };

        let mut stmt = conn.prepare(
            "SELECT status,
                    COUNT(*),
                    COALESCE(SUM(COALESCE(tokens_input, input_tokens)), 0),
                    COALESCE(SUM(tokens_input_cached), 0),
                    COALESCE(SUM(COALESCE(tokens_output, output_tokens)), 0)
             FROM segments WHERE job_id = ?1 GROUP BY status",
        )?;
        let rows = stmt.query_map(params![job_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;

        for row in rows {
            let (status, count, input_tokens, input_cached_tokens, output_tokens) = row?;
            let count = count as usize;
            summary.total_segments += count;
            summary.input_tokens += input_tokens as u64;
            summary.input_cached_tokens += input_cached_tokens as u64;
            summary.output_tokens += output_tokens as u64;
            match SegmentStatus::from_db_text(&status) {
                SegmentStatus::Succeeded => summary.succeeded += count,
                SegmentStatus::Failed => summary.failed += count,
                SegmentStatus::NeedsReview => summary.needs_review += count,
                SegmentStatus::RetryPending => summary.retry_pending += count,
                SegmentStatus::SkippedCached => summary.cached += count,
                // Unknown legacy values still count toward the totals above;
                // they just cannot be bucketed into a lifecycle column.
                SegmentStatus::Unknown(_) | SegmentStatus::Queued => {}
            }
        }

        summary.retried = conn.query_row(
            "SELECT COUNT(*) FROM segments WHERE job_id = ?1 AND attempts > 1",
            params![job_id],
            |row| row.get::<_, i64>(0),
        )? as usize;

        Ok(Some(summary))
    }

    /// All job ids, most-recently-created first.
    pub fn list_job_ids(&self) -> Result<Vec<String>> {
        let conn = self.conn.borrow();
        let mut stmt = conn.prepare("SELECT id FROM jobs ORDER BY created_at DESC")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(row?);
        }
        Ok(ids)
    }

    /// Every job paired with its aggregated segment [`JobSummary`], newest
    /// first. Powers the `watch` job picker and any dashboard job list.
    ///
    /// Runs in three queries total (jobs, per-`(job, status)` segment
    /// aggregates, retried counts) rather than a per-job N+1 of `get_job` +
    /// `summary`, which each scanned the segments table again for every job.
    pub fn list_job_summaries(&self) -> Result<Vec<(JobRecord, JobSummary)>> {
        let conn = self.conn.borrow();

        let mut job_stmt = conn.prepare(&format!(
            "SELECT {} FROM jobs ORDER BY created_at DESC",
            Self::JOB_COLUMNS
        ))?;
        let jobs = job_stmt
            .query_map([], Self::job_record_from_row)?
            .collect::<rusqlite::Result<Vec<JobRecord>>>()?;

        // One pass over the segments table, aggregated per (job, status).
        let mut aggregates: HashMap<String, JobSummary> = HashMap::new();
        let mut seg_stmt = conn.prepare(
            "SELECT job_id, status,
                    COUNT(*),
                    COALESCE(SUM(COALESCE(tokens_input, input_tokens)), 0),
                    COALESCE(SUM(tokens_input_cached), 0),
                    COALESCE(SUM(COALESCE(tokens_output, output_tokens)), 0)
             FROM segments GROUP BY job_id, status",
        )?;
        let seg_rows = seg_stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?;
        for row in seg_rows {
            let (job_id, status, count, input_tokens, input_cached_tokens, output_tokens) = row?;
            let summary = aggregates.entry(job_id).or_default();
            let count = count as usize;
            summary.total_segments += count;
            summary.input_tokens += input_tokens as u64;
            summary.input_cached_tokens += input_cached_tokens as u64;
            summary.output_tokens += output_tokens as u64;
            match SegmentStatus::from_db_text(&status) {
                SegmentStatus::Succeeded => summary.succeeded += count,
                SegmentStatus::Failed => summary.failed += count,
                SegmentStatus::NeedsReview => summary.needs_review += count,
                SegmentStatus::RetryPending => summary.retry_pending += count,
                SegmentStatus::SkippedCached => summary.cached += count,
                // Unknown legacy values still count toward the totals above;
                // they just cannot be bucketed into a lifecycle column.
                SegmentStatus::Unknown(_) | SegmentStatus::Queued => {}
            }
        }

        // Retried counts, again in one grouped pass.
        let mut retried_stmt = conn
            .prepare("SELECT job_id, COUNT(*) FROM segments WHERE attempts > 1 GROUP BY job_id")?;
        let retried_rows = retried_stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        for row in retried_rows {
            let (job_id, retried) = row?;
            aggregates.entry(job_id).or_default().retried = retried as usize;
        }

        Ok(jobs
            .into_iter()
            .map(|job| {
                let mut summary = aggregates.remove(&job.id).unwrap_or_default();
                summary.id = job.id.clone();
                summary.status = job.status.clone();
                (job, summary)
            })
            .collect())
    }

    pub(super) fn touch_job(&self, job_id: &str, status: JobStatus) -> Result<()> {
        let conn = self.conn.borrow();
        ensure_job_exists(&conn, job_id)?;
        conn.execute(
            "UPDATE jobs SET status = ?1, updated_at = ?2 WHERE id = ?3",
            params![status.as_db_text(), timestamp_string(), job_id],
        )?;
        Ok(())
    }

    pub(super) fn touch_job_unless_status(
        &self,
        job_id: &str,
        status: JobStatus,
        protected_statuses: &[JobStatus],
    ) -> Result<()> {
        let conn = self.conn.borrow();
        ensure_job_exists(&conn, job_id)?;
        touch_job_unless_status_on(&conn, job_id, status, protected_statuses)
    }
}

/// Fail closed when a mutation targets a job row that does not exist: a
/// silent zero-row UPDATE would otherwise let a typo'd or already-pruned job
/// be "mutated" without ever surfacing (STORE lifecycle audit).
pub(super) fn ensure_job_exists(conn: &Connection, job_id: &str) -> Result<()> {
    let exists = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM jobs WHERE id = ?1)",
        params![job_id],
        |row| row.get::<_, i64>(0),
    )? != 0;
    if exists {
        Ok(())
    } else {
        Err(StoreError::NotFound(format!(
            "job '{job_id}' was not found"
        )))
    }
}

pub(super) fn segment_exists(conn: &Connection, job_id: &str, segment_id: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM segments WHERE job_id = ?1 AND id = ?2)",
        params![job_id, segment_id],
        |row| row.get::<_, i64>(0),
    )? != 0)
}

/// Connection-scoped variant of [`JobStore::touch_job_unless_status`] so the
/// per-segment checkpoint can update the job inside its own transaction.
pub(super) fn touch_job_unless_status_on(
    conn: &Connection,
    job_id: &str,
    status: JobStatus,
    protected_statuses: &[JobStatus],
) -> Result<()> {
    let now = timestamp_string();
    if protected_statuses.is_empty() {
        conn.execute(
            "UPDATE jobs SET status = ?1, updated_at = ?2 WHERE id = ?3",
            params![status.as_db_text(), now, job_id],
        )?;
        return Ok(());
    }

    let placeholders = std::iter::repeat_n("?", protected_statuses.len())
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "UPDATE jobs
         SET status = ?, updated_at = ?
         WHERE id = ? AND status NOT IN ({placeholders})"
    );
    // Owned copies keep the referenced values alive until execute; the texts
    // themselves are constant identifiers, never user input.
    let status_text = status.as_db_text().to_string();
    let protected_texts: Vec<String> = protected_statuses
        .iter()
        .map(|protected| protected.as_db_text().to_string())
        .collect();
    let mut params: Vec<&dyn rusqlite::types::ToSql> =
        Vec::with_capacity(3 + protected_texts.len());
    params.push(&status_text);
    params.push(&now);
    params.push(&job_id);
    for protected in &protected_texts {
        params.push(protected);
    }
    conn.execute(&sql, params.as_slice())?;
    Ok(())
}
