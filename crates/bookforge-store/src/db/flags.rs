use super::*;

#[derive(Debug, Clone, Copy)]
pub enum RetryScope {
    Failed,
    NeedsReview,
    All,
}

impl JobStore {
    pub fn retry_segments(&self, job_id: &str, scope: RetryScope) -> Result<usize> {
        let where_status = match scope {
            RetryScope::Failed => "status = 'failed'",
            RetryScope::NeedsReview => "status = 'needs_review'",
            RetryScope::All => "status IN ('failed', 'needs_review')",
        };
        let sql = format!(
            "UPDATE segments SET status = 'retry_pending', error = NULL WHERE job_id = ?1 AND {where_status}"
        );
        let count = {
            let conn = self.conn.borrow();
            conn.execute(&sql, params![job_id])?
        };
        // Findings are instrumentation, so a failed findings write must never
        // fail the surrounding translation checkpoint.
        let _ = self.prune_stale_findings(job_id);
        self.touch_job_unless_status(job_id, "retry_pending", &["stopped"])?;
        Ok(count)
    }

    pub fn request_segment_retry(
        &self,
        job_id: &str,
        segment_id: &str,
        guidance: Option<&str>,
    ) -> Result<()> {
        if self.translation_is_human_corrected(job_id, segment_id)? {
            return Err(StoreError::InvalidCorrection(format!(
                "segment '{segment_id}' has a frozen human correction"
            )));
        }
        let mut conn = self.conn.borrow_mut();
        let tx = conn.transaction()?;
        let job_status = tx
            .query_row(
                "SELECT status FROM jobs WHERE id = ?1",
                params![job_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(job_status) = &job_status
            && matches!(job_status.as_str(), "running" | "paused")
        {
            return Err(StoreError::InvalidCorrection(format!(
                "job '{job_id}' is {job_status}; stop it before requesting a retry"
            )));
        }
        let updated = tx.execute(
            "UPDATE segments SET status = 'retry_pending', error = NULL
             WHERE job_id = ?1 AND id = ?2",
            params![job_id, segment_id],
        )?;
        if updated == 0 {
            return Err(StoreError::InvalidCorrection(format!(
                "segment '{segment_id}' was not found in job '{job_id}'"
            )));
        }
        tx.execute(
            "DELETE FROM segment_flags
             WHERE job_id = ?1 AND segment_id = ?2 AND kind = 'dashboard_retry'",
            params![job_id, segment_id],
        )?;
        tx.execute(
            "DELETE FROM qa_findings WHERE job_id = ?1 AND segment_id = ?2",
            params![job_id, segment_id],
        )?;
        if let Some(guidance) = guidance.filter(|value| !value.trim().is_empty()) {
            tx.execute(
                "INSERT INTO segment_flags
                 (job_id, segment_id, kind, note, ingested_at, consumed)
                 VALUES (?1, ?2, 'dashboard_retry', ?3, ?4, 0)",
                params![job_id, segment_id, guidance.trim(), timestamp_string()],
            )?;
        }
        tx.commit()?;
        drop(conn);
        self.touch_job_unless_status(job_id, "retry_pending", &["stopped"])?;
        Ok(())
    }

    pub fn load_retry_guidance(&self, job_id: &str) -> Result<HashMap<String, String>> {
        let conn = self.conn.borrow();
        let mut stmt = conn.prepare(
            "SELECT segment_id, note FROM segment_flags
             WHERE job_id = ?1 AND kind = 'dashboard_retry' AND consumed = 0
                   AND note IS NOT NULL AND TRIM(note) <> ''
             ORDER BY id",
        )?;
        let rows = stmt.query_map(params![job_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut guidance = HashMap::new();
        for row in rows {
            let (segment_id, note) = row?;
            guidance.insert(segment_id, note);
        }
        Ok(guidance)
    }

    pub fn set_dashboard_segment_flag(
        &self,
        job_id: &str,
        segment_id: &str,
        flagged: bool,
    ) -> Result<()> {
        let conn = self.conn.borrow();
        let exists = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM segments WHERE job_id = ?1 AND id = ?2)",
            params![job_id, segment_id],
            |row| row.get::<_, i64>(0),
        )? != 0;
        if !exists {
            return Err(StoreError::InvalidCorrection(format!(
                "segment '{segment_id}' was not found in job '{job_id}'"
            )));
        }
        conn.execute(
            "DELETE FROM segment_flags
             WHERE job_id = ?1 AND segment_id = ?2 AND kind = 'dashboard_flag'",
            params![job_id, segment_id],
        )?;
        if flagged {
            conn.execute(
                "INSERT INTO segment_flags
                 (job_id, segment_id, kind, ingested_at, consumed)
                 VALUES (?1, ?2, 'dashboard_flag', ?3, 0)",
                params![job_id, segment_id, timestamp_string()],
            )?;
        }
        Ok(())
    }

    pub fn dashboard_flagged_segment_ids(&self, job_id: &str) -> Result<Vec<String>> {
        let conn = self.conn.borrow();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT segment_id FROM segment_flags
             WHERE job_id = ?1 AND kind = 'dashboard_flag' AND consumed = 0
             ORDER BY segment_id",
        )?;
        let rows = stmt.query_map(params![job_id], |row| row.get::<_, String>(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn insert_segment_flags(&self, flags: &[NewSegmentFlag<'_>]) -> Result<usize> {
        let mut conn = self.conn.borrow_mut();
        let tx = conn.transaction()?;
        let ingested_at = timestamp_string();
        let mut inserted = 0usize;
        for flag in flags {
            inserted += tx.execute(
                "INSERT INTO segment_flags
                 (job_id, segment_id, kind, note, suggested_source, suggested_target, ingested_at, consumed)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    flag.job_id,
                    flag.segment_id,
                    flag.kind,
                    flag.note,
                    flag.suggested_source,
                    flag.suggested_target,
                    ingested_at,
                    if flag.consumed { 1_i64 } else { 0_i64 },
                ],
            )?;
        }
        tx.commit()?;
        Ok(inserted)
    }

    pub fn mark_segments_needs_review(
        &self,
        job_id: &str,
        segment_ids: &[String],
        reason: &str,
    ) -> Result<usize> {
        const SQLITE_IN_CHUNK_SIZE: usize = 900;
        let mut updated = 0usize;
        for chunk in segment_ids.chunks(SQLITE_IN_CHUNK_SIZE) {
            if chunk.is_empty() {
                continue;
            }
            let placeholders = std::iter::repeat_n("?", chunk.len())
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "UPDATE segments
                 SET status = 'needs_review',
                     error = ?
                 WHERE job_id = ?
                   AND id IN ({placeholders})"
            );
            let conn = self.conn.borrow();
            let mut params: Vec<&dyn rusqlite::types::ToSql> = Vec::with_capacity(chunk.len() + 2);
            params.push(&reason);
            params.push(&job_id);
            for id in chunk {
                params.push(id);
            }
            updated += conn.execute(&sql, params.as_slice())?;
        }
        for segment_id in segment_ids {
            // Findings are instrumentation, so a failed findings write must
            // never fail the surrounding translation checkpoint.
            let _ = self.record_segment_findings(job_id, segment_id, reason);
        }
        if updated > 0 {
            self.mark_job_needs_review(job_id)?;
        }
        Ok(updated)
    }

    pub fn segment_flag_count(&self, job_id: &str) -> Result<usize> {
        let conn = self.conn.borrow();
        let count = conn.query_row(
            "SELECT COUNT(*) FROM segment_flags WHERE job_id = ?1",
            params![job_id],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(count as usize)
    }
}
