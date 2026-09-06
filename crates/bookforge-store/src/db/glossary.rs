use super::*;

impl JobStore {
    pub fn upsert_glossary_terms(&self, terms: &[GlossaryTerm]) -> Result<usize> {
        let mut conn = self.conn.borrow_mut();
        let tx = conn.transaction()?;
        let now = timestamp_string();
        let mut changed = 0usize;
        for term in terms {
            let existing_id = tx
                .query_row(
                    "SELECT id FROM glossary_terms
                     WHERE scope_kind = ?1
                       AND ((?2 IS NULL AND scope_id IS NULL) OR scope_id = ?2)
                       AND source_text = ?3
                       AND source_language = ?4
                       AND target_language = ?5",
                    params![
                        term.scope_kind.as_str(),
                        term.scope_id.as_deref(),
                        term.source_text,
                        term.source_language,
                        term.target_language,
                    ],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?;
            if let Some(id) = existing_id {
                changed += tx.execute(
                    "UPDATE glossary_terms
                     SET target_text = ?1,
                         category = ?2,
                         notes = ?3,
                         case_sensitive = ?4,
                         always_active = ?5,
                         status = ?6,
                         source_count = ?7,
                         updated_at = ?8
                     WHERE id = ?9",
                    params![
                        term.target_text,
                        term.category.as_str(),
                        term.notes.as_deref(),
                        if term.case_sensitive { 1_i64 } else { 0_i64 },
                        if term.always_active { 1_i64 } else { 0_i64 },
                        term.status.as_str(),
                        term.source_count as i64,
                        now,
                        id,
                    ],
                )?;
            } else {
                changed += tx.execute(
                    "INSERT INTO glossary_terms
                     (scope_kind, scope_id, source_text, target_text, category, notes,
                      case_sensitive, always_active, status, source_language, target_language,
                      source_count, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13)",
                    params![
                        term.scope_kind.as_str(),
                        term.scope_id.as_deref(),
                        term.source_text,
                        term.target_text,
                        term.category.as_str(),
                        term.notes.as_deref(),
                        if term.case_sensitive { 1_i64 } else { 0_i64 },
                        if term.always_active { 1_i64 } else { 0_i64 },
                        term.status.as_str(),
                        term.source_language,
                        term.target_language,
                        term.source_count as i64,
                        now,
                    ],
                )?;
            }
        }
        tx.commit()?;
        Ok(changed)
    }

    /// Insert or update a single glossary term and return its row id. The
    /// upsert and the id read are one statement, so a concurrent writer
    /// cannot make the returned id point at a different row than the one
    /// this call wrote (the former re-select after a separate transaction).
    /// The conflict target is omitted so whichever uniqueness constraint
    /// fires — the table constraint for scoped rows or the partial unique
    /// index for global rows — routes into the same DO UPDATE arm.
    pub fn add_glossary_term(&self, term: &GlossaryTerm) -> Result<i64> {
        let conn = self.conn.borrow();
        let now = timestamp_string();
        let id = conn.query_row(
            "INSERT INTO glossary_terms
             (scope_kind, scope_id, source_text, target_text, category, notes,
              case_sensitive, always_active, status, source_language, target_language,
              source_count, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13)
             ON CONFLICT DO UPDATE SET
               target_text = excluded.target_text,
               category = excluded.category,
               notes = excluded.notes,
               case_sensitive = excluded.case_sensitive,
               always_active = excluded.always_active,
               status = excluded.status,
               source_count = excluded.source_count,
               updated_at = excluded.updated_at
             RETURNING id",
            params![
                term.scope_kind.as_str(),
                term.scope_id.as_deref(),
                term.source_text,
                term.target_text,
                term.category.as_str(),
                term.notes.as_deref(),
                if term.case_sensitive { 1_i64 } else { 0_i64 },
                if term.always_active { 1_i64 } else { 0_i64 },
                term.status.as_str(),
                term.source_language,
                term.target_language,
                term.source_count as i64,
                now,
            ],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(id)
    }

    pub fn upsert_glossary_candidates(
        &self,
        book_id: &str,
        source_language: &str,
        target_language: &str,
        candidates: &[NewGlossaryCandidate<'_>],
    ) -> Result<GlossaryCandidateUpsertResult> {
        let mut conn = self.conn.borrow_mut();
        let tx = conn.transaction()?;
        let now = timestamp_string();
        let mut result = GlossaryCandidateUpsertResult::default();

        for candidate in candidates {
            let existing = tx
                .query_row(
                    "SELECT id, status FROM glossary_terms
                     WHERE scope_kind = 'book'
                       AND scope_id = ?1
                       AND source_text = ?2
                       AND source_language = ?3
                       AND target_language = ?4",
                    params![
                        book_id,
                        candidate.source_text,
                        source_language,
                        target_language
                    ],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()?;

            match existing {
                Some((id, status)) if status == GlossaryStatus::AutoCandidate.as_str() => {
                    tx.execute(
                        "UPDATE glossary_terms
                         SET category = ?1,
                             source_count = ?2,
                             updated_at = ?3
                         WHERE id = ?4",
                        params![
                            candidate.category.as_str(),
                            candidate.source_count as i64,
                            now,
                            id,
                        ],
                    )?;
                    result.updated += 1;
                }
                Some(_) => {
                    result.skipped += 1;
                }
                None => {
                    tx.execute(
                        "INSERT INTO glossary_terms
                         (scope_kind, scope_id, source_text, target_text, category, notes,
                          case_sensitive, always_active, status, source_language, target_language,
                          source_count, created_at, updated_at)
                         VALUES ('book', ?1, ?2, NULL, ?3, NULL, 1, 0, 'auto_candidate',
                                 ?4, ?5, ?6, ?7, ?7)",
                        params![
                            book_id,
                            candidate.source_text,
                            candidate.category.as_str(),
                            source_language,
                            target_language,
                            candidate.source_count as i64,
                            now,
                        ],
                    )?;
                    result.inserted += 1;
                }
            }
        }

        tx.commit()?;
        Ok(result)
    }

    pub fn list_glossary_candidate_language_pairs(
        &self,
        book_id: &str,
    ) -> Result<Vec<(String, String)>> {
        let conn = self.conn.borrow();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT source_language, target_language
             FROM glossary_terms
             WHERE scope_kind = 'book'
               AND scope_id = ?1
               AND status = 'auto_candidate'
             ORDER BY source_language, target_language",
        )?;
        let rows = stmt.query_map(params![book_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn list_glossary_candidates(
        &self,
        book_id: &str,
        source_language: &str,
        target_language: &str,
    ) -> Result<Vec<StoredGlossaryCandidate>> {
        let conn = self.conn.borrow();
        let mut stmt = conn.prepare(
            "SELECT id, source_text, target_text, category, notes, case_sensitive,
                    always_active, status, source_language, target_language, source_count
             FROM glossary_terms
             WHERE scope_kind = 'book'
               AND scope_id = ?1
               AND source_language = ?2
               AND target_language = ?3
               AND status = 'auto_candidate'
             ORDER BY source_count DESC, source_text",
        )?;
        let rows = stmt.query_map(
            params![book_id, source_language, target_language],
            glossary_candidate_from_row,
        )?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn accept_glossary_candidate(&self, id: i64, target_text: Option<&str>) -> Result<bool> {
        let conn = self.conn.borrow();
        let Some((source_text, existing_target)) = conn
            .query_row(
                "SELECT source_text, target_text
                 FROM glossary_terms
                 WHERE id = ?1 AND status = 'auto_candidate'",
                params![id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()?
        else {
            return Ok(false);
        };
        let target = target_text
            .filter(|value| !value.trim().is_empty())
            .map(ToOwned::to_owned)
            .or_else(|| existing_target.filter(|value| !value.trim().is_empty()))
            .unwrap_or(source_text);
        let updated = conn.execute(
            "UPDATE glossary_terms
             SET target_text = ?1,
                 status = 'accepted',
                 updated_at = ?2
             WHERE id = ?3 AND status = 'auto_candidate'",
            params![target, timestamp_string(), id],
        )?;
        Ok(updated > 0)
    }

    pub fn reject_glossary_candidate(&self, id: i64) -> Result<bool> {
        let conn = self.conn.borrow();
        let updated = conn.execute(
            "UPDATE glossary_terms
             SET status = 'rejected',
                 updated_at = ?1
             WHERE id = ?2 AND status = 'auto_candidate'",
            params![timestamp_string(), id],
        )?;
        Ok(updated > 0)
    }

    pub fn list_glossary_terms(&self, filter: GlossaryFilter<'_>) -> Result<Vec<GlossaryTerm>> {
        let conn = self.conn.borrow();
        let mut sql = String::from(
            "SELECT id, scope_kind, scope_id, source_text, COALESCE(target_text, ''), category, notes,
                    case_sensitive, always_active, status, source_language, target_language,
                    source_count
             FROM glossary_terms
             WHERE 1 = 1",
        );
        let mut values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        if let Some(scope_kind) = filter.scope_kind {
            sql.push_str(" AND scope_kind = ?");
            values.push(Box::new(scope_kind.as_str().to_string()));
        }
        if let Some(scope_id) = filter.scope_id {
            sql.push_str(" AND scope_id = ?");
            values.push(Box::new(scope_id.to_string()));
        }
        if let Some(source_language) = filter.source_language {
            sql.push_str(" AND source_language = ?");
            values.push(Box::new(source_language.to_string()));
        }
        if let Some(target_language) = filter.target_language {
            sql.push_str(" AND target_language = ?");
            values.push(Box::new(target_language.to_string()));
        }
        if filter.active_only {
            sql.push_str(" AND status IN ('user_seeded', 'accepted')");
        }
        sql.push_str(
            " ORDER BY source_language, target_language, scope_kind, scope_id, source_text",
        );
        let param_refs = values
            .iter()
            .map(|value| value.as_ref())
            .collect::<Vec<&dyn rusqlite::types::ToSql>>();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(param_refs.as_slice(), glossary_term_from_row)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn load_active_glossary_terms(
        &self,
        source_language: &str,
        target_language: &str,
        book_id: Option<&str>,
        series_id: Option<&str>,
    ) -> Result<Vec<GlossaryTerm>> {
        let conn = self.conn.borrow();
        let mut stmt = conn.prepare(
            "SELECT id, scope_kind, scope_id, source_text, COALESCE(target_text, ''), category, notes,
                    case_sensitive, always_active, status, source_language, target_language,
                    source_count
             FROM glossary_terms
             WHERE source_language = ?1
               AND target_language = ?2
               AND status IN ('user_seeded', 'accepted')
               AND (
                    scope_kind = 'global'
                    OR (scope_kind = 'series' AND scope_id = ?3)
                    OR (scope_kind = 'book' AND scope_id = ?4)
               )
             ORDER BY scope_kind, scope_id, source_text",
        )?;
        let rows = stmt.query_map(
            params![source_language, target_language, series_id, book_id],
            glossary_term_from_row,
        )?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn load_active_glossary_terms_for_target(
        &self,
        target_language: &str,
        book_id: Option<&str>,
        series_id: Option<&str>,
    ) -> Result<Vec<GlossaryTerm>> {
        let conn = self.conn.borrow();
        let mut stmt = conn.prepare(
            "SELECT id, scope_kind, scope_id, source_text, COALESCE(target_text, ''), category, notes,
                    case_sensitive, always_active, status, source_language, target_language,
                    source_count
             FROM glossary_terms
             WHERE target_language = ?1
               AND status IN ('user_seeded', 'accepted')
               AND (
                    scope_kind = 'global'
                    OR (scope_kind = 'series' AND scope_id = ?2)
                    OR (scope_kind = 'book' AND scope_id = ?3)
               )
             ORDER BY scope_kind, scope_id, source_language, source_text",
        )?;
        let rows = stmt.query_map(
            params![target_language, series_id, book_id],
            glossary_term_from_row,
        )?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn remove_glossary_term(&self, id: i64) -> Result<usize> {
        let conn = self.conn.borrow();
        conn.execute("DELETE FROM glossary_terms WHERE id = ?1", params![id])
            .map_err(StoreError::from)
    }

    pub fn clear_glossary_scope(
        &self,
        scope_kind: GlossaryScopeKind,
        scope_id: Option<&str>,
    ) -> Result<usize> {
        let conn = self.conn.borrow();
        let count = if scope_kind == GlossaryScopeKind::Global {
            conn.execute("DELETE FROM glossary_terms WHERE scope_kind = 'global'", [])?
        } else {
            conn.execute(
                "DELETE FROM glossary_terms WHERE scope_kind = ?1 AND scope_id = ?2",
                params![scope_kind.as_str(), scope_id],
            )?
        };
        Ok(count)
    }

    /// Upsert a style sheet for a (scope, target_language) tuple. Returns
    /// the row id of the inserted/updated row.
    pub fn upsert_style_sheet(&self, record: &NewStyleSheet<'_>) -> Result<i64> {
        let conn = self.conn.borrow();
        let now = timestamp_string();
        let updated = conn.execute(
            "UPDATE style_sheets
             SET content_toml = ?1,
                 fingerprint = ?2,
                 updated_at = ?3
             WHERE scope_kind = ?4
               AND ((?5 IS NULL AND scope_id IS NULL) OR scope_id = ?5)
               AND target_language = ?6",
            params![
                record.content_toml,
                record.fingerprint,
                &now,
                record.scope_kind.as_str(),
                record.scope_id,
                record.target_language,
            ],
        )?;
        if updated == 0 {
            conn.execute(
            "INSERT INTO style_sheets
                (scope_kind, scope_id, target_language, content_toml, fingerprint, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
             ON CONFLICT(scope_kind, scope_id, target_language) DO UPDATE SET
                content_toml = excluded.content_toml,
                fingerprint = excluded.fingerprint,
                updated_at = excluded.updated_at",
            params![
                record.scope_kind.as_str(),
                record.scope_id,
                record.target_language,
                record.content_toml,
                record.fingerprint,
                    &now,
            ],
            )?;
        }
        let id: i64 = conn.query_row(
            "SELECT id FROM style_sheets
                WHERE scope_kind = ?1 AND IFNULL(scope_id, '') = IFNULL(?2, '')
                  AND target_language = ?3",
            params![
                record.scope_kind.as_str(),
                record.scope_id,
                record.target_language
            ],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    /// Load all style sheets that apply for a given language pair and
    /// optional book/series scopes. Caller is responsible for merging via
    /// [`bookforge_core::style::merge_style_sheets`].
    pub fn load_active_style_sheets(
        &self,
        target_language: &str,
        book_id: Option<&str>,
        series_id: Option<&str>,
    ) -> Result<Vec<StoredStyleSheet>> {
        let conn = self.conn.borrow();
        let mut stmt = conn.prepare(
            "SELECT id, scope_kind, scope_id, target_language, content_toml, fingerprint
             FROM style_sheets
             WHERE target_language = ?1
               AND ( (scope_kind = 'global')
                  OR (scope_kind = 'series' AND scope_id = ?2)
                  OR (scope_kind = 'book' AND scope_id = ?3) )",
        )?;
        let rows = stmt.query_map(params![target_language, series_id, book_id], |row| {
            Ok(StoredStyleSheet {
                id: row.get(0)?,
                scope_kind: parse_row_enum(&row.get::<_, String>(1)?, 1)?,
                scope_id: row.get(2)?,
                target_language: row.get(3)?,
                content_toml: row.get(4)?,
                fingerprint: row.get(5)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn list_style_sheets(
        &self,
        target_language: Option<&str>,
        scope_kind: Option<GlossaryScopeKind>,
        scope_id: Option<&str>,
    ) -> Result<Vec<StoredStyleSheet>> {
        let conn = self.conn.borrow();
        let mut stmt = conn.prepare(
            "SELECT id, scope_kind, scope_id, target_language, content_toml, fingerprint
             FROM style_sheets
             WHERE (?1 IS NULL OR target_language = ?1)
               AND (?2 IS NULL OR scope_kind = ?2)
               AND (?3 IS NULL OR scope_id = ?3)
             ORDER BY scope_kind, scope_id",
        )?;
        let scope_text = scope_kind.map(|s| s.as_str().to_string());
        let rows = stmt.query_map(params![target_language, scope_text, scope_id], |row| {
            Ok(StoredStyleSheet {
                id: row.get(0)?,
                scope_kind: parse_row_enum(&row.get::<_, String>(1)?, 1)?,
                scope_id: row.get(2)?,
                target_language: row.get(3)?,
                content_toml: row.get(4)?,
                fingerprint: row.get(5)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn clear_style_scope(
        &self,
        scope_kind: GlossaryScopeKind,
        scope_id: Option<&str>,
    ) -> Result<usize> {
        let conn = self.conn.borrow();
        let count = if scope_kind == GlossaryScopeKind::Global {
            conn.execute("DELETE FROM style_sheets WHERE scope_kind = 'global'", [])?
        } else {
            conn.execute(
                "DELETE FROM style_sheets WHERE scope_kind = ?1 AND scope_id = ?2",
                params![scope_kind.as_str(), scope_id],
            )?
        };
        Ok(count)
    }

    pub fn upsert_entities(&self, entities: &[NewEntity<'_>]) -> Result<usize> {
        if entities.is_empty() {
            return Ok(0);
        }
        let mut conn = self.conn.borrow_mut();
        let tx = conn.transaction()?;
        let now = timestamp_string();
        let mut changed = 0usize;
        for entity in entities {
            let updated = tx.execute(
                "UPDATE entities
                 SET target_name = ?1,
                     gender_target = ?2,
                     role = ?3,
                     notes = ?4,
                     updated_at = ?5
                 WHERE scope_kind = ?6
                   AND ((?7 IS NULL AND scope_id IS NULL) OR scope_id = ?7)
                   AND source_name = ?8
                   AND source_language = ?9
                   AND target_language = ?10",
                params![
                    entity.target_name,
                    entity.gender_target.map(|g| g.as_short()),
                    entity.role,
                    entity.notes,
                    &now,
                    entity.scope_kind.as_str(),
                    entity.scope_id,
                    entity.source_name,
                    entity.source_language,
                    entity.target_language,
                ],
            )?;
            if updated == 0 {
                tx.execute(
                    "INSERT INTO entities
                        (scope_kind, scope_id, source_name, target_name, gender_target,
                         role, notes, source_language, target_language, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)
                     ON CONFLICT(scope_kind, scope_id, source_name, source_language, target_language)
                     DO UPDATE SET
                        target_name = excluded.target_name,
                        gender_target = excluded.gender_target,
                        role = excluded.role,
                        notes = excluded.notes,
                        updated_at = excluded.updated_at",
                params![
                    entity.scope_kind.as_str(),
                    entity.scope_id,
                    entity.source_name,
                    entity.target_name,
                    entity.gender_target.map(|g| g.as_short()),
                    entity.role,
                    entity.notes,
                    entity.source_language,
                    entity.target_language,
                        &now,
                ],
                )?;
            }
            changed += 1;
        }
        tx.commit()?;
        Ok(changed)
    }

    /// Load all entities that apply for a language pair and optional
    /// book/series scopes. Caller is responsible for merging via
    /// [`bookforge_core::entity::merge_scope_entities`].
    pub fn load_active_entities(
        &self,
        source_language: &str,
        target_language: &str,
        book_id: Option<&str>,
        series_id: Option<&str>,
    ) -> Result<Vec<StoredEntity>> {
        let conn = self.conn.borrow();
        let mut stmt = conn.prepare(
            "SELECT id, scope_kind, scope_id, source_name, target_name, gender_target,
                    role, notes, source_language, target_language
             FROM entities
             WHERE source_language = ?1
               AND target_language = ?2
               AND ( (scope_kind = 'global')
                  OR (scope_kind = 'series' AND scope_id = ?3)
                  OR (scope_kind = 'book' AND scope_id = ?4) )",
        )?;
        let rows = stmt.query_map(
            params![source_language, target_language, series_id, book_id],
            |row| {
                let gender: Option<String> = row.get(5)?;
                Ok(StoredEntity {
                    id: row.get(0)?,
                    scope_kind: parse_row_enum(&row.get::<_, String>(1)?, 1)?,
                    scope_id: row.get(2)?,
                    source_name: row.get(3)?,
                    target_name: row.get(4)?,
                    gender_target: gender.and_then(|g| parse_gender_short(&g)),
                    role: row.get(6)?,
                    notes: row.get(7)?,
                    source_language: row.get(8)?,
                    target_language: row.get(9)?,
                })
            },
        )?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn list_entities(
        &self,
        source_language: Option<&str>,
        target_language: Option<&str>,
        scope_kind: Option<GlossaryScopeKind>,
        scope_id: Option<&str>,
    ) -> Result<Vec<StoredEntity>> {
        let conn = self.conn.borrow();
        let mut stmt = conn.prepare(
            "SELECT id, scope_kind, scope_id, source_name, target_name, gender_target,
                    role, notes, source_language, target_language
             FROM entities
             WHERE (?1 IS NULL OR source_language = ?1)
               AND (?2 IS NULL OR target_language = ?2)
               AND (?3 IS NULL OR scope_kind = ?3)
               AND (?4 IS NULL OR scope_id = ?4)
             ORDER BY scope_kind, scope_id, source_name",
        )?;
        let scope_text = scope_kind.map(|s| s.as_str().to_string());
        let rows = stmt.query_map(
            params![source_language, target_language, scope_text, scope_id],
            |row| {
                let gender: Option<String> = row.get(5)?;
                Ok(StoredEntity {
                    id: row.get(0)?,
                    scope_kind: parse_row_enum(&row.get::<_, String>(1)?, 1)?,
                    scope_id: row.get(2)?,
                    source_name: row.get(3)?,
                    target_name: row.get(4)?,
                    gender_target: gender.and_then(|g| parse_gender_short(&g)),
                    role: row.get(6)?,
                    notes: row.get(7)?,
                    source_language: row.get(8)?,
                    target_language: row.get(9)?,
                })
            },
        )?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn clear_entities_scope(
        &self,
        scope_kind: GlossaryScopeKind,
        scope_id: Option<&str>,
    ) -> Result<usize> {
        let conn = self.conn.borrow();
        let count = if scope_kind == GlossaryScopeKind::Global {
            conn.execute("DELETE FROM entities WHERE scope_kind = 'global'", [])?
        } else {
            conn.execute(
                "DELETE FROM entities WHERE scope_kind = ?1 AND scope_id = ?2",
                params![scope_kind.as_str(), scope_id],
            )?
        };
        Ok(count)
    }
}

fn glossary_term_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<GlossaryTerm> {
    let scope_kind_text: String = row.get(1)?;
    let category_text: String = row.get(5)?;
    let status_text: String = row.get(9)?;
    Ok(GlossaryTerm {
        id: Some(row.get(0)?),
        scope_kind: parse_row_enum(&scope_kind_text, 1)?,
        scope_id: row.get(2)?,
        source_text: row.get(3)?,
        target_text: row.get(4)?,
        category: parse_row_enum(&category_text, 5)?,
        notes: row.get(6)?,
        case_sensitive: row.get::<_, i64>(7)? != 0,
        always_active: row.get::<_, i64>(8)? != 0,
        status: parse_row_enum(&status_text, 9)?,
        source_language: row.get(10)?,
        target_language: row.get(11)?,
        source_count: row.get::<_, Option<i64>>(12)?.unwrap_or(0).max(0) as usize,
    })
}

fn glossary_candidate_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<StoredGlossaryCandidate> {
    let category_text: String = row.get(3)?;
    let status_text: String = row.get(7)?;
    Ok(StoredGlossaryCandidate {
        id: row.get(0)?,
        source_text: row.get(1)?,
        target_text: row.get(2)?,
        category: parse_row_enum(&category_text, 3)?,
        notes: row.get(4)?,
        case_sensitive: row.get::<_, i64>(5)? != 0,
        always_active: row.get::<_, i64>(6)? != 0,
        status: parse_row_enum(&status_text, 7)?,
        source_language: row.get(8)?,
        target_language: row.get(9)?,
        source_count: row.get::<_, Option<i64>>(10)?.unwrap_or(0).max(0) as usize,
    })
}

fn parse_gender_short(value: &str) -> Option<EntityGender> {
    match value {
        "m" => Some(EntityGender::Masculine),
        "f" => Some(EntityGender::Feminine),
        "n" => Some(EntityGender::Neuter),
        _ => None,
    }
}

fn parse_row_enum<T>(value: &str, column: usize) -> rusqlite::Result<T>
where
    T: FromStr<Err = String>,
{
    value.parse::<T>().map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(column, Type::Text, Box::new(RowEnumError(err)))
    })
}

#[derive(Debug)]
struct RowEnumError(String);

impl std::fmt::Display for RowEnumError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RowEnumError {}
