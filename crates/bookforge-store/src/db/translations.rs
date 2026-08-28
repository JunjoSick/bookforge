use super::*;
use rusqlite::TransactionBehavior;

/// Model-write upsert for `translations`. Unlike the former
/// `INSERT OR REPLACE` — which deletes and reinserts the row — the
/// `DO UPDATE ... WHERE human_corrected = 0` guard leaves a frozen
/// human-correction row untouched at the SQL level, so even a writer that
/// skipped the application-level check cannot clobber `origin = 'manual'`,
/// `human_corrected`, or `corrected_at`.
pub(super) const MODEL_TRANSLATION_UPSERT: &str = "
INSERT INTO translations
  (segment_id, job_id, translated_text, provider, model, prompt_version, created_at)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
ON CONFLICT(job_id, segment_id) DO UPDATE SET
  translated_text = excluded.translated_text,
  provider = excluded.provider,
  model = excluded.model,
  prompt_version = excluded.prompt_version,
  created_at = excluded.created_at
WHERE human_corrected = 0";

impl JobStore {
    pub fn insert_segments(
        &self,
        job_id: &str,
        segments: &[Segment],
        prompt_version: &str,
        provider: &str,
        model: &str,
        cache_namespace: &str,
    ) -> Result<()> {
        let mut conn = self.conn.borrow_mut();
        let tx = conn.transaction()?;
        let queued_status = SegmentStatus::Queued.as_db_text();
        for segment in segments {
            // Resume re-runs this against rows that already exist. The
            // conflict arm refreshes the cache-attribution identity columns
            // to the current run's config so a resume after a provider/model
            // change cannot leave stale values that future cache lookups
            // would misattribute; status/attempts/tokens stay untouched.
            tx.execute(
                &format!(
                    "INSERT INTO segments
                     (id, job_id, section_id, ordinal, source_hash, prompt_version, provider, model, status, attempts, cache_namespace)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, '{queued_status}', 0, ?9)
                     ON CONFLICT(job_id, id) DO UPDATE SET
                       source_hash = excluded.source_hash,
                       prompt_version = excluded.prompt_version,
                       provider = excluded.provider,
                       model = excluded.model,
                       cache_namespace = excluded.cache_namespace"
                ),
                params![
                    segment.id.0,
                    job_id,
                    segment.section_id.0,
                    segment.ordinal as i64,
                    segment.checksum,
                    prompt_version,
                    provider,
                    model,
                    cache_namespace,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn save_translation(&self, request: SaveTranslation<'_>) -> Result<()> {
        self.save_translation_with_findings(request, None)
    }

    /// Save a successful translation and optionally replace its durable QA
    /// findings. The segment remains `succeeded`; warnings live in
    /// `qa_findings`, not `segments.error`.
    ///
    /// The whole checkpoint — freeze check, translation row, block rows,
    /// segment record, findings, retry-guidance consumption, and job touch —
    /// commits as ONE `IMMEDIATE` transaction: the write lock is taken up
    /// front, so no other process can interleave a human correction between
    /// the check and the write (STORE-1), and a crash can never leave the
    /// per-segment checkpoint half-applied (STORE-3).
    pub fn save_translation_with_findings(
        &self,
        request: SaveTranslation<'_>,
        findings: Option<&str>,
    ) -> Result<()> {
        let now = timestamp_string();
        let translated_hash = stable_hash(request.translated_text);
        let mut conn = self.conn.borrow_mut();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if translation_is_human_corrected_on(&tx, request.job_id, request.segment_id)? {
            return Ok(());
        }
        tx.execute(
            MODEL_TRANSLATION_UPSERT,
            params![
                request.segment_id,
                request.job_id,
                request.translated_text,
                request.provider,
                request.model,
                request.prompt_version,
                now
            ],
        )?;
        replace_block_translations(&tx, request.job_id, request.segment_id, request.blocks)?;
        tx.execute(
            &format!(
                "UPDATE segments
                 SET status = '{}',
                     attempts = attempts + 1,
                     tokens_input = ?1,
                     tokens_input_cached = ?2,
                     tokens_output = ?3,
                     tokens_estimated = ?4,
                     translated_hash = ?5,
                     error = NULL
                 WHERE job_id = ?6 AND id = ?7",
                SegmentStatus::Succeeded.as_db_text()
            ),
            params![
                request.input_tokens.map(|value| value as i64),
                request.input_cached_tokens.map(|value| value as i64),
                request.output_tokens.map(|value| value as i64),
                if request.tokens_estimated {
                    1_i64
                } else {
                    0_i64
                },
                translated_hash,
                request.job_id,
                request.segment_id,
            ],
        )?;
        // Findings are instrumentation, so a failed findings write must never
        // fail the surrounding translation checkpoint.
        let findings = findings.map(str::trim).filter(|value| !value.is_empty());
        let _ = match findings {
            Some(findings) => {
                record_segment_findings_on(&tx, request.job_id, request.segment_id, findings)
                    .map(|_| ())
            }
            None => clear_segment_findings_on(&tx, request.job_id, request.segment_id),
        };
        consume_dashboard_retry_guidance_on(&tx, request.job_id, request.segment_id)?;
        touch_job_unless_status_on(
            &tx,
            request.job_id,
            JobStatus::Running,
            &[JobStatus::Paused, JobStatus::Stopped],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Save a preserved needs-review translation. Like
    /// [`JobStore::save_translation_with_findings`], the entire per-segment
    /// checkpoint commits as one `IMMEDIATE` transaction guarded against
    /// clobbering a frozen human correction.
    pub fn save_needs_review(&self, request: SaveNeedsReview<'_>) -> Result<()> {
        let now = timestamp_string();
        let translated_hash = stable_hash(request.preserved_text);
        let mut conn = self.conn.borrow_mut();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if translation_is_human_corrected_on(&tx, request.job_id, request.segment_id)? {
            return Ok(());
        }
        tx.execute(
            MODEL_TRANSLATION_UPSERT,
            params![
                request.segment_id,
                request.job_id,
                request.preserved_text,
                request.provider,
                request.model,
                request.prompt_version,
                now
            ],
        )?;
        replace_block_translations(&tx, request.job_id, request.segment_id, request.blocks)?;
        tx.execute(
            &format!(
                "UPDATE segments
                 SET status = '{}',
                     attempts = attempts + 1,
                     tokens_input = ?1,
                     tokens_input_cached = ?2,
                     tokens_output = ?3,
                     tokens_estimated = ?4,
                     translated_hash = ?5,
                     error = ?6
                 WHERE job_id = ?7 AND id = ?8",
                SegmentStatus::NeedsReview.as_db_text()
            ),
            params![
                request.input_tokens.map(|value| value as i64),
                request.input_cached_tokens.map(|value| value as i64),
                request.output_tokens.map(|value| value as i64),
                if request.tokens_estimated {
                    1_i64
                } else {
                    0_i64
                },
                translated_hash,
                request.error,
                request.job_id,
                request.segment_id
            ],
        )?;
        // Findings are instrumentation, so a failed findings write must never
        // fail the surrounding translation checkpoint.
        let _ = record_segment_findings_on(&tx, request.job_id, request.segment_id, request.error);
        consume_dashboard_retry_guidance_on(&tx, request.job_id, request.segment_id)?;
        touch_job_unless_status_on(
            &tx,
            request.job_id,
            JobStatus::NeedsReview,
            &[JobStatus::Paused, JobStatus::Stopped],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Save a cache-hit translation. Like the other model-write paths, this
    /// is one `IMMEDIATE` transaction that yields to frozen human corrections.
    pub fn save_cached_translation(&self, request: SaveCachedTranslation<'_>) -> Result<()> {
        let now = timestamp_string();
        let translated_hash = stable_hash(request.translated_text);
        let mut conn = self.conn.borrow_mut();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if translation_is_human_corrected_on(&tx, request.job_id, request.segment_id)? {
            return Ok(());
        }
        tx.execute(
            MODEL_TRANSLATION_UPSERT,
            params![
                request.segment_id,
                request.job_id,
                request.translated_text,
                request.provider,
                request.model,
                request.prompt_version,
                now
            ],
        )?;
        replace_block_translations(&tx, request.job_id, request.segment_id, request.blocks)?;
        tx.execute(
            &format!(
                "UPDATE segments
                 SET status = '{}',
                     tokens_input = NULL,
                     tokens_input_cached = NULL,
                     tokens_output = NULL,
                     tokens_estimated = 0,
                     translated_hash = ?1,
                     error = NULL
                 WHERE job_id = ?2 AND id = ?3",
                SegmentStatus::SkippedCached.as_db_text()
            ),
            params![translated_hash, request.job_id, request.segment_id],
        )?;
        // Findings are instrumentation, so a failed findings write must never
        // fail the surrounding translation checkpoint.
        let _ = clear_segment_findings_on(&tx, request.job_id, request.segment_id);
        consume_dashboard_retry_guidance_on(&tx, request.job_id, request.segment_id)?;
        touch_job_unless_status_on(
            &tx,
            request.job_id,
            JobStatus::Running,
            &[JobStatus::Paused, JobStatus::Stopped],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn save_manual_correction(&self, request: SaveManualCorrection<'_>) -> Result<()> {
        if request.translated_text.trim().is_empty() {
            return Err(StoreError::InvalidCorrection(
                "translation text cannot be empty".to_string(),
            ));
        }
        if request.blocks.is_empty()
            || request
                .blocks
                .iter()
                .any(|block| block.text.trim().is_empty())
        {
            return Err(StoreError::InvalidCorrection(
                "every corrected block must contain text".to_string(),
            ));
        }

        let now = timestamp_string();
        let translated_hash = stable_hash(request.translated_text);
        let mut conn = self.conn.borrow_mut();
        // IMMEDIATE so the policy checks below and the correction write are
        // atomic against other processes sharing the database file.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let job_status = tx
            .query_row(
                "SELECT status FROM jobs WHERE id = ?1",
                params![request.job_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(job_status) = job_status else {
            return Err(StoreError::NotFound(format!(
                "job '{}' was not found",
                request.job_id
            )));
        };
        match JobStatus::from_db_text(&job_status) {
            JobStatus::Running | JobStatus::Paused => {
                return Err(StoreError::InvalidCorrection(format!(
                    "job '{}' is {job_status}; stop it before applying a manual correction",
                    request.job_id
                )));
            }
            _ => {}
        }

        let prompt_version = tx
            .query_row(
                "SELECT prompt_version FROM segments WHERE job_id = ?1 AND id = ?2",
                params![request.job_id, request.segment_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(prompt_version) = prompt_version else {
            return Err(StoreError::NotFound(format!(
                "segment '{}' was not found in job '{}'",
                request.segment_id, request.job_id
            )));
        };

        tx.execute(
            "INSERT INTO translations
             (segment_id, job_id, translated_text, provider, model, prompt_version, created_at,
              origin, human_corrected, corrected_at)
             VALUES (?1, ?2, ?3, 'manual', 'manual', ?4, ?5, 'manual', 1, ?5)
             ON CONFLICT(job_id, segment_id) DO UPDATE SET
               translated_text = excluded.translated_text,
               provider = 'manual',
               model = 'manual',
               prompt_version = excluded.prompt_version,
               created_at = excluded.created_at,
               origin = 'manual',
               human_corrected = 1,
               corrected_at = excluded.corrected_at",
            params![
                request.segment_id,
                request.job_id,
                request.translated_text,
                prompt_version,
                now,
            ],
        )?;
        replace_block_translations(&tx, request.job_id, request.segment_id, request.blocks)?;
        tx.execute(
            &format!(
                "UPDATE segments
                 SET status = '{}', translated_hash = ?1, error = NULL
                 WHERE job_id = ?2 AND id = ?3",
                SegmentStatus::Succeeded.as_db_text()
            ),
            params![translated_hash, request.job_id, request.segment_id],
        )?;
        tx.execute(
            "DELETE FROM qa_findings WHERE job_id = ?1 AND segment_id = ?2",
            params![request.job_id, request.segment_id],
        )?;
        tx.execute(
            "UPDATE segment_flags SET consumed = 1 WHERE job_id = ?1 AND segment_id = ?2",
            params![request.job_id, request.segment_id],
        )?;
        tx.commit()?;
        drop(conn);

        self.recompute_job_status(request.job_id)?;
        Ok(())
    }

    pub fn translation_is_human_corrected(&self, job_id: &str, segment_id: &str) -> Result<bool> {
        let conn = self.conn.borrow();
        translation_is_human_corrected_on(&conn, job_id, segment_id)
    }

    pub fn pending_segment_ids(&self, job_id: &str) -> Result<Vec<String>> {
        let conn = self.conn.borrow();
        let sql = format!(
            "SELECT id FROM segments
             WHERE job_id = ?1 AND status IN ({})
             ORDER BY ordinal",
            SegmentStatus::sql_set(&[SegmentStatus::Queued, SegmentStatus::RetryPending])
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![job_id], |row| row.get::<_, String>(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn segment_records(&self, job_id: &str) -> Result<Vec<SegmentRecord>> {
        let conn = self.conn.borrow();
        let mut stmt = conn.prepare(
            "SELECT id,
                    status,
                    attempts,
                    error,
                    COALESCE(tokens_input, input_tokens),
                    tokens_input_cached,
                    COALESCE(tokens_output, output_tokens),
                    tokens_estimated
             FROM segments WHERE job_id = ?1 ORDER BY ordinal",
        )?;
        let rows = stmt.query_map(params![job_id], |row| {
            Ok(SegmentRecord {
                id: row.get(0)?,
                status: row.get(1)?,
                attempts: row.get::<_, i64>(2)? as usize,
                error: row.get(3)?,
                input_tokens: row.get::<_, Option<i64>>(4)?.map(|value| value as u64),
                input_cached_tokens: row.get::<_, Option<i64>>(5)?.map(|value| value as u64),
                output_tokens: row.get::<_, Option<i64>>(6)?.map(|value| value as u64),
                tokens_estimated: row.get::<_, i64>(7)? != 0,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn load_block_translations(&self, job_id: &str) -> Result<Vec<StoredBlockTranslation>> {
        let conn = self.conn.borrow();
        let mut stmt = conn.prepare(
            "SELECT segment_id, block_id, translated_text
             FROM translation_blocks WHERE job_id = ?1 ORDER BY segment_id, block_id",
        )?;
        let rows = stmt.query_map(params![job_id], |row| {
            Ok(StoredBlockTranslation {
                segment_id: row.get(0)?,
                block_id: row.get(1)?,
                text: row.get(2)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn load_terminal_segment_translations(
        &self,
        job_id: &str,
    ) -> Result<Vec<StoredSegmentTranslation>> {
        let conn = self.conn.borrow();
        let sql = format!(
            "SELECT s.id, s.ordinal, s.status, s.error, t.translated_text,
                    t.provider, t.model, t.human_corrected, t.corrected_at
             FROM segments s
             JOIN translations t ON t.job_id = s.job_id AND t.segment_id = s.id
             WHERE s.job_id = ?1 AND s.status IN ({})
             ORDER BY s.ordinal",
            SegmentStatus::sql_set(SegmentStatus::terminal_with_translation())
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![job_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, i64>(7)? != 0,
                row.get::<_, Option<String>>(8)?,
            ))
        })?;

        let mut records = Vec::new();
        for row in rows {
            let (
                segment_id,
                ordinal,
                status,
                error,
                translated_text,
                provider,
                model,
                human_corrected,
                corrected_at,
            ) = row?;
            let mut block_stmt = conn.prepare(
                "SELECT block_id, translated_text
                 FROM translation_blocks
                 WHERE job_id = ?1 AND segment_id = ?2
                 ORDER BY block_id",
            )?;
            let blocks = block_stmt
                .query_map(params![job_id, segment_id.as_str()], |row| {
                    Ok(BlockTranslation {
                        block_id: BlockId(row.get::<_, String>(0)?),
                        text: row.get(1)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            records.push(StoredSegmentTranslation {
                segment_id,
                ordinal: ordinal as usize,
                status,
                error,
                translated_text,
                blocks,
                provider,
                model,
                human_corrected,
                corrected_at,
            });
        }

        Ok(records)
    }

    pub fn resumable_segment_ids(&self, job_id: &str) -> Result<Vec<String>> {
        let conn = self.conn.borrow();
        let sql = format!(
            "SELECT id FROM segments
             WHERE job_id = ?1 AND status IN ({})
             ORDER BY ordinal",
            SegmentStatus::sql_set(SegmentStatus::resumable())
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![job_id], |row| row.get::<_, String>(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn find_cached_translation(
        &self,
        segment: &Segment,
        prompt_version: &str,
        provider: &str,
        model: &str,
        source_lang: Option<&str>,
        target_lang: &str,
        cache_namespace: &str,
    ) -> Result<Option<CachedTranslation>> {
        let conn = self.conn.borrow();
        let sql = format!(
            "SELECT t.job_id, t.segment_id, t.translated_text
             FROM translations t
             JOIN segments s ON s.job_id = t.job_id AND s.id = t.segment_id
             JOIN jobs j ON j.id = t.job_id
             WHERE s.source_hash = ?1
               AND s.prompt_version = ?2
               AND s.provider = ?3
               AND s.model = ?4
               AND ((?5 IS NULL AND j.source_lang IS NULL) OR j.source_lang = ?5)
               AND j.target_lang = ?6
               AND s.cache_namespace = ?7
               AND s.status IN ({})
               AND t.human_corrected = 0
             ORDER BY CASE s.status WHEN '{}' THEN 0 ELSE 1 END,
                      CAST(t.created_at AS INTEGER) DESC,
                      t.rowid DESC
             LIMIT 1",
            SegmentStatus::sql_set(SegmentStatus::resolved()),
            SegmentStatus::Succeeded.as_db_text()
        );
        let cached = conn
            .query_row(
                &sql,
                params![
                    segment.checksum,
                    prompt_version,
                    provider,
                    model,
                    source_lang,
                    target_lang,
                    cache_namespace,
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;

        let Some((job_id, segment_id, translated_text)) = cached else {
            return Ok(None);
        };

        let mut stmt = conn.prepare(
            "SELECT block_id, translated_text
             FROM translation_blocks
             WHERE job_id = ?1 AND segment_id = ?2
             ORDER BY block_id",
        )?;
        let rows = stmt.query_map(params![job_id, segment_id], |row| {
            Ok(BlockTranslation {
                block_id: BlockId(row.get::<_, String>(0)?),
                text: row.get(1)?,
            })
        })?;
        let blocks = rows.collect::<std::result::Result<Vec<_>, _>>()?;

        let mut by_id = blocks
            .into_iter()
            .map(|block| (block.block_id.0.clone(), block))
            .collect::<HashMap<_, _>>();

        let mut ordered = Vec::with_capacity(segment.block_ids.len());
        for id in &segment.block_ids {
            let Some(block) = by_id.remove(&id.0) else {
                return Ok(None);
            };
            ordered.push(block);
        }
        if !by_id.is_empty() {
            return Ok(None);
        }

        Ok(Some(CachedTranslation {
            translated_text,
            blocks: ordered,
        }))
    }

    pub fn find_cached_translations_batch(
        &self,
        segments: &[Segment],
        request: CacheLookupRequest<'_>,
    ) -> Result<HashMap<String, CachedTranslation>> {
        let mut results = HashMap::new();
        if segments.is_empty() {
            return Ok(results);
        }

        const SQLITE_IN_CHUNK_SIZE: usize = 900;

        for chunk in segments.chunks(SQLITE_IN_CHUNK_SIZE) {
            let hashes: Vec<&str> = chunk.iter().map(|s| s.checksum.as_str()).collect();
            let placeholders: Vec<String> =
                (0..hashes.len()).map(|i| format!("?{}", i + 1)).collect();
            let placeholders_sql = placeholders.join(", ");

            let sql = format!(
                "SELECT t.job_id, t.segment_id, t.translated_text, s.source_hash
                 FROM translations t
                 JOIN segments s ON s.job_id = t.job_id AND s.id = t.segment_id
                 JOIN jobs j ON j.id = t.job_id
                 WHERE s.source_hash IN ({placeholders_sql})
                   AND s.prompt_version = ?{}
                   AND s.provider = ?{}
                   AND s.model = ?{}
                   AND ((?{} IS NULL AND j.source_lang IS NULL) OR j.source_lang = ?{})
                   AND j.target_lang = ?{}
                   AND s.cache_namespace = ?{}
                   AND s.status IN ({})
                   AND t.human_corrected = 0
                 ORDER BY CASE s.status WHEN '{}' THEN 0 ELSE 1 END,
                          CAST(t.created_at AS INTEGER) DESC,
                          t.rowid DESC",
                hashes.len() + 1,
                hashes.len() + 2,
                hashes.len() + 3,
                hashes.len() + 4,
                hashes.len() + 5,
                hashes.len() + 6,
                hashes.len() + 7,
                SegmentStatus::sql_set(SegmentStatus::resolved()),
                SegmentStatus::Succeeded.as_db_text()
            );

            let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
            for hash in &hashes {
                params.push(Box::new(hash.to_string()));
            }
            params.push(Box::new(request.prompt_version.to_string()));
            params.push(Box::new(request.provider.to_string()));
            params.push(Box::new(request.model.to_string()));
            params.push(Box::new(request.source_lang.map(|s| s.to_string())));
            params.push(Box::new(request.source_lang.map(|s| s.to_string())));
            params.push(Box::new(request.target_lang.to_string()));
            params.push(Box::new(request.cache_namespace.to_string()));

            let conn = self.conn.borrow();
            let mut stmt = conn.prepare(&sql)?;
            let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                params.iter().map(|p| p.as_ref()).collect();

            let rows = stmt.query_map(param_refs.as_slice(), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?;

            let mut hash_to_hit: HashMap<String, (String, String, String)> = HashMap::new();
            for row in rows {
                let (job_id, segment_id, translated_text, source_hash) = row?;
                hash_to_hit
                    .entry(source_hash)
                    .or_insert((job_id, segment_id, translated_text));
            }

            for segment in chunk {
                if let Some((job_id, segment_id, translated_text)) =
                    hash_to_hit.get(&segment.checksum)
                {
                    let mut block_stmt = conn.prepare(
                        "SELECT block_id, translated_text
                         FROM translation_blocks
                         WHERE job_id = ?1 AND segment_id = ?2
                         ORDER BY block_id",
                    )?;
                    let block_rows = block_stmt.query_map(params![job_id, segment_id], |row| {
                        Ok(BlockTranslation {
                            block_id: BlockId(row.get::<_, String>(0)?),
                            text: row.get(1)?,
                        })
                    })?;
                    let blocks = block_rows.collect::<std::result::Result<Vec<_>, _>>()?;

                    let mut by_id = blocks
                        .into_iter()
                        .map(|block| (block.block_id.0.clone(), block))
                        .collect::<HashMap<_, _>>();

                    let mut ordered = Vec::with_capacity(segment.block_ids.len());
                    let mut valid = true;
                    for id in &segment.block_ids {
                        let Some(block) = by_id.remove(&id.0) else {
                            valid = false;
                            break;
                        };
                        ordered.push(block);
                    }
                    if !valid || !by_id.is_empty() {
                        continue;
                    }

                    results.insert(
                        segment.id.0.clone(),
                        CachedTranslation {
                            translated_text: translated_text.clone(),
                            blocks: ordered,
                        },
                    );
                }
            }
        }

        Ok(results)
    }
}

/// Connection-scoped freeze check so the model-write paths and
/// `request_segment_retry` can run it inside their own `IMMEDIATE`
/// transaction.
pub(super) fn translation_is_human_corrected_on(
    conn: &Connection,
    job_id: &str,
    segment_id: &str,
) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT human_corrected FROM translations WHERE job_id = ?1 AND segment_id = ?2",
            params![job_id, segment_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .is_some_and(|value| value != 0))
}

fn consume_dashboard_retry_guidance_on(
    conn: &Connection,
    job_id: &str,
    segment_id: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE segment_flags SET consumed = 1
         WHERE job_id = ?1 AND segment_id = ?2 AND kind = 'dashboard_retry'",
        params![job_id, segment_id],
    )?;
    Ok(())
}

/// Connection-scoped variant of [`JobStore::record_segment_findings`] so the
/// per-segment checkpoint can write findings inside its own transaction.
///
/// Legacy error strings carry no block attribution, so these rows persist
/// `block_id = NULL`; block-level findings arrive via
/// [`record_segment_engine_findings_on`].
pub(super) fn record_segment_findings_on(
    conn: &Connection,
    job_id: &str,
    segment_id: &str,
    error: &str,
) -> Result<usize> {
    let findings = classify_segment_error(error);
    let exists = conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM segments WHERE job_id = ?1 AND id = ?2
         )",
        params![job_id, segment_id],
        |row| row.get::<_, bool>(0),
    )?;
    if !exists {
        return Ok(0);
    }

    conn.execute(
        "DELETE FROM qa_findings
         WHERE job_id = ?1 AND segment_id = ?2 AND kind NOT GLOB 'llm_*'",
        params![job_id, segment_id],
    )?;
    for (index, finding) in findings.iter().enumerate() {
        let hash = stable_hash(&format!("{job_id}\u{1f}{segment_id}\u{1f}{index}"));
        let id = format!("qaf_{}", &hash[..24]);
        insert_qa_finding_row(
            conn,
            NewQaFindingRow {
                id: &id,
                segment_id,
                job_id,
                severity: finding.severity.as_str(),
                kind: finding.kind.as_str(),
                message: &finding.message,
                block_id: finding.block_id.as_deref(),
            },
        )?;
    }
    Ok(findings.len())
}

/// Connection-scoped variant of [`JobStore::record_segment_engine_findings`]
/// so per-segment checkpoints can write the canonical structured findings —
/// with block attribution and per-instance severity — inside their own
/// transaction.
pub(super) fn record_segment_engine_findings_on(
    conn: &Connection,
    job_id: &str,
    segment_id: &str,
    findings: &[EngineFinding],
) -> Result<usize> {
    let exists = conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM segments WHERE job_id = ?1 AND id = ?2
         )",
        params![job_id, segment_id],
        |row| row.get::<_, bool>(0),
    )?;
    if !exists {
        return Ok(0);
    }

    conn.execute(
        "DELETE FROM qa_findings
         WHERE job_id = ?1 AND segment_id = ?2 AND kind NOT GLOB 'llm_*'",
        params![job_id, segment_id],
    )?;
    for (index, finding) in findings.iter().enumerate() {
        let hash = stable_hash(&format!(
            "{job_id}\u{1f}engine\u{1f}{segment_id}\u{1f}{index}"
        ));
        let id = format!("qaf_{}", &hash[..24]);
        insert_qa_finding_row(
            conn,
            NewQaFindingRow {
                id: &id,
                segment_id,
                job_id,
                severity: finding.severity.as_str(),
                kind: finding.kind.as_str(),
                message: &finding.message,
                block_id: finding.block_id.as_deref(),
            },
        )?;
    }
    Ok(findings.len())
}

/// Connection-scoped variant of [`JobStore::clear_segment_findings`].
pub(super) fn clear_segment_findings_on(
    conn: &Connection,
    job_id: &str,
    segment_id: &str,
) -> Result<()> {
    conn.execute(
        "DELETE FROM qa_findings
         WHERE job_id = ?1 AND segment_id = ?2 AND kind NOT GLOB 'llm_*'",
        params![job_id, segment_id],
    )?;
    Ok(())
}

fn replace_block_translations(
    conn: &Connection,
    job_id: &str,
    segment_id: &str,
    blocks: &[BlockTranslation],
) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM translation_blocks WHERE job_id = ?1 AND segment_id = ?2",
        params![job_id, segment_id],
    )?;
    for block in blocks {
        conn.execute(
            "INSERT INTO translation_blocks (segment_id, job_id, block_id, translated_text)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                segment_id,
                job_id,
                block.block_id.0.as_str(),
                block.text.as_str()
            ],
        )?;
    }
    Ok(())
}
