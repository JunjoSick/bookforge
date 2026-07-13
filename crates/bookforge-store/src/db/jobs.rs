use super::*;

impl JobStore {
    pub fn create_job(&self, request: CreateJob<'_>) -> Result<JobRecord> {
        let input_hash = file_hash(request.input)?;
        let id = format!("job_{}_{}", unix_timestamp_nanos(), &input_hash[..12]);
        let now = timestamp_string();
        let input_path = request.input.to_path_buf();
        let output_path = request.output.to_path_buf();
        let conn = self.conn.borrow();
        conn.execute(
            "INSERT INTO jobs
             (id, input_path, output_path, input_hash, source_lang, target_lang, provider, model, base_url, api_key_env, book_id, series_id, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 'running', ?13, ?13)",
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
            status: "running".to_string(),
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
        conn.execute(
            "UPDATE jobs SET output_path = ?1, updated_at = ?2 WHERE id = ?3",
            params![path.to_string_lossy(), timestamp_string(), job_id],
        )?;
        Ok(())
    }

    pub fn recompute_job_status(&self, job_id: &str) -> Result<()> {
        let conn = self.conn.borrow();
        let (total, unresolved) = conn.query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(CASE WHEN status IN ('succeeded', 'skipped_cached') THEN 0 ELSE 1 END), 0)
             FROM segments WHERE job_id = ?1",
            params![job_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )?;
        drop(conn);
        if total > 0 && unresolved == 0 {
            self.touch_job(job_id, "succeeded")
        } else {
            self.touch_job(job_id, "needs_review")
        }
    }

    pub fn mark_job_complete(&self, job_id: &str) -> Result<()> {
        self.touch_job_unless_status(job_id, "succeeded", &["stopped"])
    }

    pub fn mark_job_running(&self, job_id: &str) -> Result<()> {
        self.touch_job_unless_status(job_id, "running", &["stopped"])
    }

    pub fn mark_job_running_for_resume(&self, job_id: &str) -> Result<()> {
        self.touch_job(job_id, "running")
    }

    pub fn mark_job_paused(&self, job_id: &str) -> Result<()> {
        self.touch_job_unless_status(job_id, "paused", &["stopped"])
    }

    pub fn mark_job_stopped(&self, job_id: &str) -> Result<()> {
        self.touch_job(job_id, "stopped")
    }

    pub fn mark_job_succeeded(&self, job_id: &str) -> Result<()> {
        self.mark_job_complete(job_id)
    }

    pub fn mark_job_needs_review(&self, job_id: &str) -> Result<()> {
        self.touch_job_unless_status(job_id, "needs_review", &["stopped"])
    }

    pub fn mark_job_interrupted(&self, job_id: &str) -> Result<()> {
        self.touch_job_unless_status(job_id, "interrupted", &["stopped"])
    }

    pub fn mark_job_failed(&self, job_id: &str) -> Result<()> {
        self.touch_job_unless_status(job_id, "failed", &["stopped"])
    }

    pub fn mark_segment_failed(&self, job_id: &str, segment_id: &str, error: &str) -> Result<()> {
        {
            let conn = self.conn.borrow();
            conn.execute(
                "UPDATE segments SET status = 'failed', attempts = attempts + 1, error = ?1 WHERE job_id = ?2 AND id = ?3",
                params![error, job_id, segment_id],
            )?;
        }
        self.mark_job_failed(job_id)?;
        Ok(())
    }

    pub fn mark_segment_failed_if_unfinished(
        &self,
        job_id: &str,
        segment_id: &str,
        error: &str,
    ) -> Result<()> {
        {
            let conn = self.conn.borrow();
            conn.execute(
                "UPDATE segments
                 SET status = 'failed', attempts = attempts + 1, error = ?1
                 WHERE job_id = ?2
                   AND id = ?3
                   AND status NOT IN ('succeeded', 'skipped_cached', 'needs_review')",
                params![error, job_id, segment_id],
            )?;
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
        const SQLITE_IN_CHUNK_SIZE: usize = 900;
        let mut updated = 0;

        for chunk in candidate_segment_ids.chunks(SQLITE_IN_CHUNK_SIZE) {
            if chunk.is_empty() {
                continue;
            }

            let placeholders = std::iter::repeat_n("?", chunk.len())
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "UPDATE segments
                 SET status = 'failed', attempts = attempts + 1, error = ?
                 WHERE job_id = ?
                   AND id IN ({placeholders})
                   AND status NOT IN ('succeeded', 'skipped_cached', 'needs_review')"
            );

            let conn = self.conn.borrow();
            let mut params: Vec<&dyn rusqlite::types::ToSql> = Vec::with_capacity(chunk.len() + 2);
            params.push(&error);
            params.push(&job_id);
            for id in chunk {
                params.push(id);
            }
            updated += conn.execute(&sql, params.as_slice())?;
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
            match status.as_str() {
                "succeeded" => summary.succeeded += count,
                "failed" => summary.failed += count,
                "needs_review" => summary.needs_review += count,
                "retry_pending" => summary.retry_pending += count,
                "skipped_cached" => summary.cached += count,
                _ => {}
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
            match status.as_str() {
                "succeeded" => summary.succeeded += count,
                "failed" => summary.failed += count,
                "needs_review" => summary.needs_review += count,
                "retry_pending" => summary.retry_pending += count,
                "skipped_cached" => summary.cached += count,
                _ => {}
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

    pub(super) fn touch_job(&self, job_id: &str, status: &str) -> Result<()> {
        let conn = self.conn.borrow();
        conn.execute(
            "UPDATE jobs SET status = ?1, updated_at = ?2 WHERE id = ?3",
            params![status, timestamp_string(), job_id],
        )?;
        Ok(())
    }

    pub(super) fn touch_job_unless_status(
        &self,
        job_id: &str,
        status: &str,
        protected_statuses: &[&str],
    ) -> Result<()> {
        let now = timestamp_string();
        let conn = self.conn.borrow();
        if protected_statuses.is_empty() {
            conn.execute(
                "UPDATE jobs SET status = ?1, updated_at = ?2 WHERE id = ?3",
                params![status, now, job_id],
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
        let mut params: Vec<&dyn rusqlite::types::ToSql> =
            Vec::with_capacity(3 + protected_statuses.len());
        params.push(&status);
        params.push(&now);
        params.push(&job_id);
        for protected in protected_statuses {
            params.push(protected);
        }
        conn.execute(&sql, params.as_slice())?;
        Ok(())
    }
}
