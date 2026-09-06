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
        if !job_exists_on(&tx, job_id)? {
            return Err(StoreError::NotFound(format!(
                "job '{job_id}' was not found"
            )));
        }
        // The cache identity for every segment is derived once per job from
        // the persisted run snapshot + cache policy (never from a prior cache
        // row), so all rows of this run agree on the rendered inputs.
        let identity_ctx = load_cache_identity_context(&tx, job_id, segments)?;
        let queued_status = SegmentStatus::Queued.as_db_text();
        for segment in segments {
            // Stamp the single structured cache identity for this segment so
            // cache lookups match on one deterministic fingerprint that
            // captures every output-affecting input (the actual rendered
            // context, glossary/style/entity blocks, the persisted run
            // snapshot, and cache policy). Resume re-runs refresh it to the
            // current run's config, and conflict-arm refreshes never touch
            // status/attempts/tokens.
            let cache_fingerprint = cache_identity_fingerprint_for(
                &identity_ctx,
                segment,
                provider,
                model,
                prompt_version,
                cache_namespace,
            );
            tx.execute(
                &format!(
                    "INSERT INTO segments
                     (id, job_id, section_id, ordinal, source_hash, prompt_version, provider, model, status, attempts, cache_namespace, cache_fingerprint)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, '{queued_status}', 0, ?9, ?10)
                     ON CONFLICT(job_id, id) DO UPDATE SET
                       source_hash = excluded.source_hash,
                       prompt_version = excluded.prompt_version,
                       provider = excluded.provider,
                       model = excluded.model,
                       cache_namespace = excluded.cache_namespace,
                       cache_fingerprint = excluded.cache_fingerprint"
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
                    cache_fingerprint,
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
        if !segment_exists_on(&tx, request.job_id, request.segment_id)? {
            return Err(StoreError::NotFound(format!(
                "segment '{}' was not found in job '{}'",
                request.segment_id, request.job_id
            )));
        }
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
        // A fresh model write replaces the output, so any LLM-review findings
        // about the previous output are stale: drop them here so QA-off
        // reruns cannot leave old `llm_*` rows attached to re-translated
        // segments. Deterministic findings are refreshed below.
        clear_segment_llm_findings_on(&tx, request.job_id, request.segment_id)?;
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
        // No ledger row is written here: this checkpoint records segment STATE,
        // not a provider wire attempt. Real wire attempts are recorded
        // explicitly by the attempt-recording lane (see
        // `JobStore::record_translation_attempt`); deriving an attempt from the
        // checkpoint would fabricate a synthetic wire outcome and double-count
        // once the provider lane instruments its own requests.
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
        if !segment_exists_on(&tx, request.job_id, request.segment_id)? {
            return Err(StoreError::NotFound(format!(
                "segment '{}' was not found in job '{}'",
                request.segment_id, request.job_id
            )));
        }
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
        // A preserved/fresh output replaces the segment text; stale LLM QA
        // rows about the previous output are no longer valid.
        clear_segment_llm_findings_on(&tx, request.job_id, request.segment_id)?;
        // Findings are instrumentation, so a failed findings write must never
        // fail the surrounding translation checkpoint.
        let _ = record_segment_findings_on(&tx, request.job_id, request.segment_id, request.error);
        // Like the success path, no ledger row is derived from this
        // checkpoint: it records segment state, not a provider wire attempt.
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
        if !segment_exists_on(&tx, request.job_id, request.segment_id)? {
            return Err(StoreError::NotFound(format!(
                "segment '{}' was not found in job '{}'",
                request.segment_id, request.job_id
            )));
        }
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
        // The segment output is replaced by the cache-hit value; stale LLM QA
        // rows about the previous output no longer describe this segment.
        clear_segment_llm_findings_on(&tx, request.job_id, request.segment_id)?;
        // Findings are instrumentation, so a failed findings write must never
        // fail the surrounding translation checkpoint.
        let _ = clear_segment_findings_on(&tx, request.job_id, request.segment_id);
        // A cache hit is NOT a provider wire attempt, so no ledger row is
        // derived here: the ledger counts real wire traffic only.
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

    pub fn find_cached_translation(
        &self,
        segment: &Segment,
        request: CacheLookupRequest<'_>,
    ) -> Result<Option<CachedTranslation>> {
        let conn = self.conn.borrow();
        // The expected fingerprint is computed by the caller from the current
        // run's persisted config + cache policy (never discovered from a prior
        // segment row), keyed by the CURRENT segment id so duplicate source
        // text in different contexts resolves to distinct expectations.
        // Absent means the segment is ineligible for reuse.
        let Some(expected_fingerprint) = request.expected_fingerprints.get(&segment.id.0) else {
            return Ok(None);
        };

        // Match cross-job candidates on the single expected identity and the
        // actual output provenance (t.provider/t.model), so output produced
        // by a fallback provider/model is never reused as if the primary
        // produced it.
        let sql = format!(
            "SELECT t.job_id, t.segment_id, t.translated_text
             FROM translations t
             JOIN segments s ON s.job_id = t.job_id AND s.id = t.segment_id
             WHERE s.source_hash = ?1
               AND s.cache_fingerprint = ?2
               AND t.provider = ?3
               AND t.model = ?4
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
                    expected_fingerprint,
                    request.provider,
                    request.model,
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

        const SQLITE_IN_CHUNK_SIZE: usize = 450;

        for chunk in segments.chunks(SQLITE_IN_CHUNK_SIZE) {
            // Expected fingerprints are caller-computed (from the current
            // run's persisted config + cache policy), keyed by CURRENT segment
            // id. Each entry carries the segment's own source hash AND its
            // expected fingerprint, so two segments that share a checksum but
            // differ in rendered context resolve to different (hash,
            // fingerprint) pairs and can never collide. Segments without one
            // are ineligible for reuse.
            let pairs: Vec<(&str, &str, &str)> = chunk
                .iter()
                .filter_map(|segment| {
                    request
                        .expected_fingerprints
                        .get(&segment.id.0)
                        .map(|fingerprint| {
                            (
                                segment.id.0.as_str(),
                                segment.checksum.as_str(),
                                fingerprint.as_str(),
                            )
                        })
                })
                .collect();
            if pairs.is_empty() {
                continue;
            }

            // One direct lookup: candidates must carry the exact expected
            // fingerprint for their own source hash AND the actual output
            // provenance (t.provider/t.model), so a fallback-produced row is
            // never reused as if the primary produced it. The returned
            // (source_hash, cache_fingerprint) pair maps each hit back to the
            // exact requesting segment, keeping duplicate source texts apart.
            let pair_conditions = (0..pairs.len())
                .map(|index| {
                    let hash_slot = index * 2 + 1;
                    let fingerprint_slot = index * 2 + 2;
                    format!("(s.source_hash = ?{hash_slot} AND s.cache_fingerprint = ?{fingerprint_slot})")
                })
                .collect::<Vec<_>>()
                .join(" OR ");
            let sql = format!(
                "SELECT t.job_id, t.segment_id, t.translated_text, s.source_hash, s.cache_fingerprint
                 FROM translations t
                 JOIN segments s ON s.job_id = t.job_id AND s.id = t.segment_id
                 WHERE ({pair_conditions})
                   AND t.provider = ?{}
                   AND t.model = ?{}
                   AND s.status IN ({})
                   AND t.human_corrected = 0
                 ORDER BY CASE s.status WHEN '{}' THEN 0 ELSE 1 END,
                          CAST(t.created_at AS INTEGER) DESC,
                          t.rowid DESC",
                pairs.len() * 2 + 1,
                pairs.len() * 2 + 2,
                SegmentStatus::sql_set(SegmentStatus::resolved()),
                SegmentStatus::Succeeded.as_db_text()
            );

            let conn = self.conn.borrow();
            let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
            for (_, hash, fingerprint) in &pairs {
                params.push(Box::new(hash.to_string()));
                params.push(Box::new(fingerprint.to_string()));
            }
            params.push(Box::new(request.provider.to_string()));
            params.push(Box::new(request.model.to_string()));

            let mut stmt = conn.prepare(&sql)?;
            let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                params.iter().map(|p| p.as_ref()).collect();

            let rows = stmt.query_map(param_refs.as_slice(), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?;

            // A hit is only valid for a requesting segment whose (source
            // hash, expected fingerprint) pair matches exactly — never by
            // checksum alone.
            let mut hit_by_key: HashMap<(String, String), (String, String, String)> =
                HashMap::new();
            for row in rows {
                let (job_id, segment_id, translated_text, source_hash, cache_fingerprint) = row?;
                hit_by_key
                    .entry((source_hash, cache_fingerprint))
                    .or_insert((job_id, segment_id, translated_text));
            }

            for segment in chunk {
                let Some(fingerprint) = request.expected_fingerprints.get(&segment.id.0) else {
                    continue;
                };
                let Some((job_id, segment_id, translated_text)) =
                    hit_by_key.get(&(segment.checksum.clone(), fingerprint.clone()))
                else {
                    continue;
                };
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

        Ok(results)
    }

    /// Compute the expected structured cache identity fingerprint for the
    /// eligible candidate segments from the job's persisted run snapshot,
    /// cache policy, and the ACTUAL rendered prompt ingredients. This is
    /// computed from the current run's configuration — never read from
    /// segment rows.
    ///
    /// The identity is derived over the FULL ordered segment set
    /// (`all_segments`), because per-segment glossary selection depends on the
    /// ordered neighborhood (recently-active window) and the high-frequency
    /// anchors of the whole book: a resume that passes only a sparse subset of
    /// pending candidates would otherwise select different terms and compute a
    /// different identity than the original run stamped. `segments` narrows the
    /// returned map to the current lookup's eligible candidates, and the
    /// returned map is keyed by CURRENT segment id (not source checksum), so
    /// duplicate source text at different positions/context resolves to
    /// distinct expectations and can never collide.
    ///
    /// Jobs that never recorded a snapshot fall back to the request-visible
    /// minimal identity, mirroring what [`JobStore::insert_segments`] stamps.
    #[allow(clippy::too_many_arguments)]
    pub fn expected_cache_fingerprints(
        &self,
        job_id: &str,
        all_segments: &[Segment],
        segments: &[Segment],
        provider: &str,
        model: &str,
        prompt_version: &str,
        cache_namespace: &str,
    ) -> Result<HashMap<String, String>> {
        let mut fingerprints = HashMap::with_capacity(segments.len());
        let conn = self.conn.borrow();
        if segments.is_empty() {
            return Ok(fingerprints);
        }
        let identity_ctx = load_cache_identity_context(&conn, job_id, all_segments)?;
        for segment in segments {
            let fingerprint = cache_identity_fingerprint_for(
                &identity_ctx,
                segment,
                provider,
                model,
                prompt_version,
                cache_namespace,
            );
            fingerprints.insert(segment.id.0.clone(), fingerprint);
        }
        Ok(fingerprints)
    }

    /// Record one attempt in the append-only `translation_attempts` ledger.
    /// Rows are never updated or deleted: earlier attempts stay verbatim and
    /// the ledger is the provenance-of-record for cost/usage aggregation.
    pub fn record_translation_attempt(&self, request: RecordTranslationAttempt<'_>) -> Result<i64> {
        let mut conn = self.conn.borrow_mut();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !segment_exists_on(&tx, request.job_id, request.segment_id)? {
            return Err(StoreError::NotFound(format!(
                "segment '{}' was not found in job '{}'",
                request.segment_id, request.job_id
            )));
        }
        let now = timestamp_string();
        let ordinal = append_attempt_on(&tx, request, &now)?;
        tx.commit()?;
        Ok(ordinal)
    }

    /// Read back attempt rows, optionally filtered by segment and/or phase.
    /// Unknown jobs return an empty vec (matching the other read APIs).
    pub fn translation_attempts(
        &self,
        job_id: &str,
        segment_id: Option<&str>,
        phase: Option<TranslationAttemptPhase>,
    ) -> Result<Vec<TranslationAttemptRecord>> {
        let conn = self.conn.borrow();
        let mut sql = String::from(
            "SELECT id, job_id, segment_id, batch_id, phase, attempt_ordinal,
                    provider, model, outcome, error,
                    input_tokens, input_cached_tokens, output_tokens,
                    cost_estimate, created_at
             FROM translation_attempts
             WHERE job_id = ?1",
        );
        if segment_id.is_some() {
            sql.push_str(" AND segment_id = ?2");
        }
        // `phase` comes from the fixed enum vocabulary (never user input), so
        // embedding its literal mirrors the established status.sql_set pattern.
        if let Some(phase) = phase {
            sql.push_str(&format!(" AND phase = '{}'", phase.as_db_text()));
        }
        sql.push_str(" ORDER BY attempt_ordinal, id");
        let mut stmt = conn.prepare(&sql)?;
        // Bind only the parameters the built SQL actually declares: `?2` is
        // present only when a segment filter was requested.
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(job_id.to_string())];
        if let Some(segment_id) = segment_id {
            params.push(Box::new(segment_id.to_string()));
        }
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|param| param.as_ref()).collect();
        let rows = stmt.query_map(param_refs.as_slice(), |row| {
            Ok(TranslationAttemptRecord {
                id: row.get(0)?,
                job_id: row.get(1)?,
                segment_id: row.get(2)?,
                batch_id: row.get(3)?,
                phase: row.get(4)?,
                attempt_ordinal: row.get::<_, i64>(5)? as usize,
                provider: row.get(6)?,
                model: row.get(7)?,
                outcome: row.get(8)?,
                error: row.get(9)?,
                input_tokens: row.get::<_, Option<i64>>(10)?.map(|v| v as u64),
                input_cached_tokens: row.get::<_, Option<i64>>(11)?.map(|v| v as u64),
                output_tokens: row.get::<_, Option<i64>>(12)?.map(|v| v as u64),
                cost_estimate: row.get(13)?,
                created_at: row.get(14)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    /// Aggregate one job's attempts from the append-only ledger.
    pub fn translation_attempt_summary(&self, job_id: &str) -> Result<TranslationAttemptSummary> {
        let conn = self.conn.borrow();
        conn.query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(input_tokens), 0),
                    COALESCE(SUM(input_cached_tokens), 0),
                    COALESCE(SUM(output_tokens), 0),
                    COALESCE(SUM(cost_estimate), 0)
             FROM translation_attempts WHERE job_id = ?1",
            params![job_id],
            |row| {
                Ok(TranslationAttemptSummary {
                    attempts: row.get::<_, i64>(0)? as usize,
                    input_tokens: row.get::<_, i64>(1)? as u64,
                    input_cached_tokens: row.get::<_, i64>(2)? as u64,
                    output_tokens: row.get::<_, i64>(3)? as u64,
                    cost_estimate: row.get(4)?,
                })
            },
        )
        .map_err(StoreError::from)
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

/// Whether a segment row exists for `(job_id, segment_id)`. The model-write
/// paths use this to surface [`StoreError::NotFound`] instead of silently
/// succeeding (or tripping a foreign-key violation).
fn segment_exists_on(conn: &Connection, job_id: &str, segment_id: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM segments WHERE job_id = ?1 AND id = ?2)",
        params![job_id, segment_id],
        |row| row.get::<_, bool>(0),
    )?)
}

/// Everything the structured cache identity needs that is shared across all
/// segments of one job: the persisted snapshot (when present), the conservative
/// cache policy, and the per-segment glossary selection. Computed ONCE per
/// caller transaction so a book's segments do not re-read the snapshot or
/// re-run glossary selection per row.
struct CacheIdentityContext {
    source_lang: Option<String>,
    target_lang: String,
    snapshot: Option<RunConfigSnapshot>,
    strict_context: Option<bool>,
    /// Per-segment selected glossary terms (keyed by segment id) when a
    /// snapshot exists; the selection is the ACTUAL content the prompt renders
    /// for each segment, so two segments that select different terms can never
    /// collide even with identical config fingerprints.
    glossary_entries: Option<HashMap<String, Vec<GlossaryPromptTerm>>>,
}

/// Load the shared cache-identity inputs for a job's segments from the
/// persisted `RunConfigSnapshot` + cache policy. The per-segment glossary
/// selection is computed once over the whole slice because selection depends on
/// the ordered neighborhood (recently-active window), mirroring exactly what
/// the CLI renders into each prompt.
fn load_cache_identity_context(
    conn: &Connection,
    job_id: &str,
    segments: &[Segment],
) -> Result<CacheIdentityContext> {
    let (source_lang, target_lang, config_json, policy_json) = conn.query_row(
        "SELECT source_lang, target_lang, config_json, cache_policy_json FROM jobs WHERE id = ?1",
        params![job_id],
        |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        },
    )?;
    let Some(config_json) = config_json.filter(|json| !json.trim().is_empty()) else {
        return Ok(CacheIdentityContext {
            source_lang,
            target_lang,
            snapshot: None,
            strict_context: None,
            glossary_entries: None,
        });
    };
    let snapshot = serde_json::from_str::<RunConfigSnapshot>(&config_json)
        .map_err(|error| StoreError::Serialization(error.to_string()))?;
    let strict_context = parse_cache_policy_json(policy_json.as_deref())?.strict_context;
    let glossary_entries = if segments.is_empty() {
        None
    } else {
        let selection = select_glossary_for_segments(
            segments,
            &snapshot.glossary_terms,
            snapshot.glossary_budget_tokens,
        );
        Some(selection.entries_by_segment)
    };
    Ok(CacheIdentityContext {
        source_lang,
        target_lang,
        snapshot: Some(snapshot),
        strict_context,
        glossary_entries,
    })
}

/// Compute the single structured cache identity fingerprint for a segment at
/// write time. Prefers the job's persisted `RunConfigSnapshot` (full
/// output-affecting settings) combined with the ACTUAL rendered prompt
/// ingredients — the segment's own neighbor/context content and the real
/// per-segment glossary selection plus rendered style/entity blocks; falls back
/// to the request-visible minimal identity when no snapshot exists
/// (legacy/fixtures). The strict-context choice comes from the job's persisted
/// [`CachePolicySnapshot`]; absent policy reads the conservative `None`.
fn cache_identity_fingerprint_for(
    ctx: &CacheIdentityContext,
    segment: &Segment,
    provider: &str,
    model: &str,
    prompt_version: &str,
    cache_namespace: &str,
) -> String {
    let Some(snapshot) = &ctx.snapshot else {
        return CacheIdentity::minimal(MinimalCacheIdentity {
            segment,
            provider,
            model,
            source_lang: ctx.source_lang.as_deref(),
            target_lang: &ctx.target_lang,
            prompt_version,
            cache_namespace,
        })
        .fingerprint();
    };
    let glossary_rendered = ctx
        .glossary_entries
        .as_ref()
        .and_then(|entries| entries.get(&segment.id.0))
        .map(|entries| serde_json::to_string(entries).unwrap_or_default())
        .unwrap_or_default();
    let prompt_inputs = CachePromptInputs {
        glossary_rendered,
        style_rendered: snapshot.style_rendered_block.clone(),
        entities_rendered: snapshot.entities_rendered_block.clone(),
    };
    snapshot
        .cache_identity(bookforge_core::run_snapshot::CacheIdentityRequest {
            segment,
            provider,
            model,
            prompt_version,
            cache_namespace,
            strict_context: ctx.strict_context,
            prompt_inputs: &prompt_inputs,
        })
        .fingerprint()
}

/// Decode a serialized [`CachePolicySnapshot`]. NULL/absent and empty policy
/// records read back the conservative default (unknown strictness), which is
/// hashed distinctly from either explicit choice — an unknown cache policy
/// fails CLOSED and can never reuse (or be reused by) a run that states one.
/// Malformed JSON is a hard error so a corrupt policy can never silently
/// degrade to a permissive default.
fn parse_cache_policy_json(json: Option<&str>) -> Result<CachePolicySnapshot> {
    let Some(json) = json.filter(|json| !json.trim().is_empty()) else {
        return Ok(CachePolicySnapshot::conservative());
    };
    serde_json::from_str(json).map_err(|error| StoreError::Serialization(error.to_string()))
}

/// Load the persisted cache policy for a job. Rows that never recorded a
/// policy (NULL `cache_policy_json`) read back the conservative default
/// (unknown strictness), which can never match an explicit choice.
pub(super) fn load_cache_policy_on(conn: &Connection, job_id: &str) -> Result<CachePolicySnapshot> {
    let json = conn
        .query_row(
            "SELECT cache_policy_json FROM jobs WHERE id = ?1",
            params![job_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    parse_cache_policy_json(json.as_deref())
}

/// Drop persisted LLM-review findings for one segment. Called on every path
/// that replaces a segment's output, so QA-off reruns cannot leave stale
/// `llm_*` rows attached to re-translated (or cache-replaced) segments.
pub(super) fn clear_segment_llm_findings_on(
    conn: &Connection,
    job_id: &str,
    segment_id: &str,
) -> Result<()> {
    conn.execute(
        "DELETE FROM qa_findings WHERE job_id = ?1 AND segment_id = ?2 AND kind GLOB 'llm_*'",
        params![job_id, segment_id],
    )?;
    Ok(())
}

/// Phase inference for a ledger row whose phase was not stated explicitly: an
/// attempt whose provider/model match the job's primary configuration is
/// `primary`; anything else is a `fallback` attempt. Keeps the effective
/// fallback provenance accurate in the ledger.
fn infer_attempt_phase_on(
    conn: &Connection,
    job_id: &str,
    provider: &str,
    model: &str,
) -> Result<TranslationAttemptPhase> {
    let (job_provider, job_model) = conn.query_row(
        "SELECT provider, model FROM jobs WHERE id = ?1",
        params![job_id],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    )?;
    if job_provider == provider && job_model == model {
        Ok(TranslationAttemptPhase::Primary)
    } else {
        Ok(TranslationAttemptPhase::Fallback)
    }
}

/// INSERT one append-only `translation_attempts` row inside the caller's
/// transaction. `attempt_ordinal` is computed atomically by the INSERT itself
/// (one past the current maximum for the (job, segment) pair), so the
/// allocation is safe even inside a DEFERRED transaction: SQLite serializes the
/// statement under the write lock, and two concurrent writers can never derive
/// the same ordinal. Earlier attempts are never overwritten; the unique
/// `(job_id, segment_id, attempt_ordinal)` constraint makes the sequence
/// immutable and monotonic. When `request.phase` is `None` it is inferred from
/// the effective provider/model against the job's primary configuration.
fn append_attempt_on(
    conn: &Connection,
    request: RecordTranslationAttempt<'_>,
    created_at: &str,
) -> Result<i64> {
    let phase = match request.phase {
        Some(phase) => phase,
        None => infer_attempt_phase_on(conn, request.job_id, request.provider, request.model)?,
    };
    conn.execute(
        "INSERT INTO translation_attempts
         (job_id, segment_id, batch_id, phase, attempt_ordinal, provider, model, outcome, error,
          input_tokens, input_cached_tokens, output_tokens, cost_estimate, created_at)
         VALUES (?1, ?2, ?3, ?4,
                 (SELECT COALESCE(MAX(attempt_ordinal), 0) + 1
                  FROM translation_attempts
                  WHERE job_id = ?1 AND segment_id = ?2),
                 ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            request.job_id,
            request.segment_id,
            request.batch_id,
            phase.as_db_text(),
            request.provider,
            request.model,
            request.outcome.as_db_text(),
            request.error,
            request.input_tokens.map(|value| value as i64),
            request.input_cached_tokens.map(|value| value as i64),
            request.output_tokens.map(|value| value as i64),
            request.cost_estimate,
            created_at,
        ],
    )?;
    // The ordinal was assigned by the same statement that holds the write
    // lock; read it back from the row we just inserted.
    let attempt_ordinal = conn.query_row(
        "SELECT attempt_ordinal FROM translation_attempts WHERE id = ?1",
        params![conn.last_insert_rowid()],
        |row| row.get::<_, i64>(0),
    )?;
    Ok(attempt_ordinal)
}
