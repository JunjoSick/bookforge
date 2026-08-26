use super::status::job_sql_set;
use super::*;

#[cfg(unix)]
fn create_private_dir_all(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_private_dir_all(path: &Path) -> std::io::Result<()> {
    fs::create_dir_all(path)
}

pub fn run_doctor(db_path: Option<PathBuf>) -> Result<StorageDoctor> {
    let path = db_path.unwrap_or_else(|| PathBuf::from(".bookforge/jobs.sqlite"));
    let database_exists = path.exists();
    let wal_path = path.with_extension("sqlite-wal");
    let shm_path = path.with_extension("sqlite-shm");
    let wal_present = wal_path.exists();
    let shm_present = shm_path.exists();

    let (journal_mode, integrity_check, wal_sidecars_normal, note) = if database_exists {
        let conn = Connection::open(&path)?;
        let journal_mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap_or_else(|_| "unknown".to_string());
        let integrity_check: String = conn
            .pragma_query_value(None, "integrity_check", |row| row.get(0))
            .unwrap_or_else(|_| "error".to_string());
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);");

        let wal_sidecars_normal = if wal_present || shm_present {
            integrity_check == "ok"
        } else {
            true
        };

        let note = if wal_present || shm_present {
            "WAL sidecar files are normal. SQLite will recover them automatically. \
             Do not delete them manually while BookForge is running."
                .to_string()
        } else {
            String::new()
        };

        (journal_mode, integrity_check, wal_sidecars_normal, note)
    } else {
        ("unknown".to_string(), String::new(), true, String::new())
    };

    Ok(StorageDoctor {
        database_path: path,
        database_exists,
        wal_present,
        shm_present,
        journal_mode,
        integrity_check,
        wal_sidecars_normal,
        note,
    })
}

impl JobStore {
    pub fn open_default() -> Result<Self> {
        Self::open(".bookforge/jobs.sqlite")
    }

    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            create_private_dir_all(parent)?;
        }
        let conn = Connection::open(&path)?;

        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        let store = JobStore::new(conn, path);
        store.migrate()?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn migrate(&self) -> Result<()> {
        let mut conn = self.conn.borrow_mut();
        // Legacy pre-v1_0_1 schema: rename the whole set away in ONE
        // immediate transaction. A crash mid-cascade must never strand some
        // tables renamed and others not — the next open would silently
        // recreate empty tables over orphaned data.
        if table_exists(&conn, "translations")?
            && !table_has_column(&conn, "translations", "job_id")?
        {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let suffix = unix_timestamp_nanos();
            rename_table_if_exists(&tx, "qa_findings", suffix)?;
            rename_table_if_exists(&tx, "translation_blocks", suffix)?;
            rename_table_if_exists(&tx, "translations", suffix)?;
            rename_table_if_exists(&tx, "segments", suffix)?;
            rename_table_if_exists(&tx, "jobs", suffix)?;
            tx.commit()?;
        }
        conn.execute_batch(
            "
            PRAGMA foreign_keys = ON;
            CREATE TABLE IF NOT EXISTS _migrations (
              version INTEGER PRIMARY KEY,
              name TEXT NOT NULL,
              applied_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS jobs (
              id TEXT PRIMARY KEY,
              input_path TEXT NOT NULL DEFAULT '',
              input_snapshot_path TEXT,
              input_sha256 TEXT,
              output_path TEXT NOT NULL DEFAULT '',
              input_hash TEXT NOT NULL,
              source_lang TEXT,
              target_lang TEXT NOT NULL,
              provider TEXT NOT NULL,
              model TEXT NOT NULL,
              base_url TEXT,
              api_key_env TEXT,
              status TEXT NOT NULL,
              config_json TEXT,
              events_path TEXT,
              report_json_path TEXT,
              report_markdown_path TEXT,
              book_id TEXT,
              series_id TEXT,
              created_at TEXT NOT NULL,
              updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS segments (
              id TEXT NOT NULL,
              job_id TEXT NOT NULL,
              section_id TEXT NOT NULL,
              ordinal INTEGER NOT NULL,
              source_hash TEXT NOT NULL,
              prompt_version TEXT NOT NULL,
              provider TEXT NOT NULL,
              model TEXT NOT NULL,
              status TEXT NOT NULL,
              attempts INTEGER NOT NULL DEFAULT 0,
              input_tokens INTEGER,
              output_tokens INTEGER,
              tokens_input INTEGER,
              tokens_input_cached INTEGER,
              tokens_output INTEGER,
              tokens_estimated INTEGER NOT NULL DEFAULT 0,
              cost_estimate REAL,
              error TEXT,
              translated_hash TEXT,
              PRIMARY KEY (job_id, id),
              FOREIGN KEY(job_id) REFERENCES jobs(id)
            );

            CREATE TABLE IF NOT EXISTS translations (
              segment_id TEXT NOT NULL,
              job_id TEXT NOT NULL,
              translated_text TEXT NOT NULL,
              provider TEXT NOT NULL,
              model TEXT NOT NULL,
              prompt_version TEXT NOT NULL,
              created_at TEXT NOT NULL,
              origin TEXT NOT NULL DEFAULT 'model',
              human_corrected INTEGER NOT NULL DEFAULT 0,
              corrected_at TEXT,
              PRIMARY KEY (job_id, segment_id),
              FOREIGN KEY(job_id, segment_id) REFERENCES segments(job_id, id)
            );

            CREATE TABLE IF NOT EXISTS translation_blocks (
              segment_id TEXT NOT NULL,
              job_id TEXT NOT NULL,
              block_id TEXT NOT NULL,
              translated_text TEXT NOT NULL,
              PRIMARY KEY (job_id, segment_id, block_id),
              FOREIGN KEY(job_id, segment_id) REFERENCES segments(job_id, id)
            );

            CREATE TABLE IF NOT EXISTS qa_findings (
              id TEXT PRIMARY KEY,
              segment_id TEXT NOT NULL,
              job_id TEXT NOT NULL,
              severity TEXT NOT NULL,
              kind TEXT NOT NULL,
              message TEXT NOT NULL,
              FOREIGN KEY(job_id, segment_id) REFERENCES segments(job_id, id)
            );

            CREATE TABLE IF NOT EXISTS segment_flags (
              id INTEGER PRIMARY KEY,
              job_id TEXT NOT NULL,
              segment_id TEXT NOT NULL,
              kind TEXT NOT NULL,
              note TEXT,
              suggested_source TEXT,
              suggested_target TEXT,
              ingested_at TEXT NOT NULL,
              consumed INTEGER NOT NULL DEFAULT 0,
              FOREIGN KEY(job_id) REFERENCES jobs(id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS glossary_terms (
              id INTEGER PRIMARY KEY,
              scope_kind TEXT NOT NULL CHECK(scope_kind IN ('global', 'series', 'book')),
              scope_id TEXT,
              source_text TEXT NOT NULL,
              target_text TEXT,
              category TEXT NOT NULL CHECK(category IN
                ('person', 'place', 'object', 'invented', 'style', 'phrase', 'other')),
              notes TEXT,
              case_sensitive INTEGER NOT NULL DEFAULT 0,
              always_active INTEGER NOT NULL DEFAULT 0,
              status TEXT NOT NULL CHECK(status IN
                ('user_seeded', 'auto_candidate', 'accepted', 'rejected'))
                DEFAULT 'user_seeded',
              source_language TEXT NOT NULL,
              target_language TEXT NOT NULL,
              source_count INTEGER DEFAULT 0,
              created_at TEXT NOT NULL,
              updated_at TEXT NOT NULL,
              UNIQUE(scope_kind, scope_id, source_text, source_language, target_language)
            );

            CREATE TABLE IF NOT EXISTS style_sheets (
              id INTEGER PRIMARY KEY,
              scope_kind TEXT NOT NULL CHECK(scope_kind IN ('global', 'series', 'book')),
              scope_id TEXT,
              target_language TEXT NOT NULL,
              content_toml TEXT NOT NULL,
              fingerprint TEXT NOT NULL,
              created_at TEXT NOT NULL,
              updated_at TEXT NOT NULL,
              UNIQUE(scope_kind, scope_id, target_language)
            );

            CREATE TABLE IF NOT EXISTS entities (
              id INTEGER PRIMARY KEY,
              scope_kind TEXT NOT NULL CHECK(scope_kind IN ('global', 'series', 'book')),
              scope_id TEXT,
              source_name TEXT NOT NULL,
              target_name TEXT NOT NULL,
              gender_target TEXT
                CHECK(gender_target IS NULL OR gender_target IN ('m', 'f', 'n')),
              role TEXT,
              notes TEXT,
              source_language TEXT NOT NULL,
              target_language TEXT NOT NULL,
              created_at TEXT NOT NULL,
              updated_at TEXT NOT NULL,
              UNIQUE(scope_kind, scope_id, source_name, source_language, target_language)
            );
            ",
        )?;
        ensure_glossary_target_nullable(&conn)?;
        ensure_column(&conn, "jobs", "input_path", "TEXT NOT NULL DEFAULT ''")?;
        ensure_column(&conn, "jobs", "input_snapshot_path", "TEXT")?;
        ensure_column(&conn, "jobs", "input_sha256", "TEXT")?;
        ensure_column(&conn, "jobs", "output_path", "TEXT NOT NULL DEFAULT ''")?;
        ensure_column(&conn, "jobs", "base_url", "TEXT")?;
        ensure_column(&conn, "jobs", "api_key_env", "TEXT")?;
        ensure_column(&conn, "jobs", "config_json", "TEXT")?;
        ensure_column(&conn, "jobs", "events_path", "TEXT")?;
        ensure_column(&conn, "jobs", "report_json_path", "TEXT")?;
        ensure_column(&conn, "jobs", "report_markdown_path", "TEXT")?;
        ensure_column(&conn, "jobs", "book_id", "TEXT")?;
        ensure_column(&conn, "jobs", "series_id", "TEXT")?;
        ensure_column(
            &conn,
            "segments",
            "cache_namespace",
            "TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(&conn, "segments", "tokens_input", "INTEGER")?;
        ensure_column(&conn, "segments", "tokens_input_cached", "INTEGER")?;
        ensure_column(&conn, "segments", "tokens_output", "INTEGER")?;
        ensure_column(
            &conn,
            "segments",
            "tokens_estimated",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        ensure_column(
            &conn,
            "translations",
            "origin",
            "TEXT NOT NULL DEFAULT 'model'",
        )?;
        ensure_column(
            &conn,
            "translations",
            "human_corrected",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        ensure_column(&conn, "translations", "corrected_at", "TEXT")?;
        // Unlike the preceding idempotent DDL migrations, version 9 drives a
        // one-time data cleanup (duplicate global-scope rows the NULL-tolerant
        // UNIQUE constraints let through) and therefore is gated explicitly.
        if !migration_applied(&conn, 9)? {
            deduplicate_global_scope_rows(&conn)?;
            record_migration(&conn, 9, "v2_7_1_global_scope_unique_indexes")?;
        }
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_segments_cache_lookup
             ON segments(source_hash, cache_namespace, prompt_version, provider, model, status);
             CREATE INDEX IF NOT EXISTS idx_segment_flags_job
             ON segment_flags(job_id, consumed);
             CREATE INDEX IF NOT EXISTS idx_glossary_lookup
             ON glossary_terms(source_language, target_language, scope_kind, scope_id, status);
             CREATE INDEX IF NOT EXISTS idx_style_lookup
             ON style_sheets(target_language, scope_kind, scope_id);
             CREATE INDEX IF NOT EXISTS idx_entity_lookup
             ON entities(source_language, target_language, scope_kind, scope_id);
             CREATE INDEX IF NOT EXISTS idx_qa_findings_job
             ON qa_findings(job_id, kind);
             CREATE INDEX IF NOT EXISTS idx_qa_findings_segment
             ON qa_findings(job_id, segment_id);
             CREATE INDEX IF NOT EXISTS idx_jobs_created_at
             ON jobs(created_at);",
        )?;
        // Table-level UNIQUE(scope_kind, scope_id, ...) cannot enforce
        // identity for global rows: SQL compares NULLs as distinct, so every
        // global row (scope_id IS NULL) is unique by definition and concurrent
        // first-inserts could duplicate them. These partial unique indexes
        // close that hole for global scope while leaving scoped rows to the
        // table constraints.
        conn.execute_batch(
            "CREATE UNIQUE INDEX IF NOT EXISTS ux_glossary_terms_global_identity
             ON glossary_terms(source_text, source_language, target_language)
             WHERE scope_kind = 'global';
             CREATE UNIQUE INDEX IF NOT EXISTS ux_style_sheets_global_identity
             ON style_sheets(target_language)
             WHERE scope_kind = 'global';
             CREATE UNIQUE INDEX IF NOT EXISTS ux_entities_global_identity
             ON entities(source_name, source_language, target_language)
             WHERE scope_kind = 'global';",
        )?;
        record_migration(&conn, 1, "initial")?;
        record_migration(&conn, 2, "v1_0_1_input_snapshot")?;
        record_migration(&conn, 3, "v1_1_segment_flags")?;
        record_migration(&conn, 4, "v1_2_glossary_terms")?;
        record_migration(&conn, 5, "v1_2_1_nullable_glossary_candidate_targets")?;
        record_migration(&conn, 6, "v1_3_context_styles_entities")?;
        record_migration(&conn, 7, "v2_4_human_corrections")?;
        // Unlike the preceding idempotent DDL migrations, version 8 drives a
        // one-time data backfill and therefore must be gated explicitly.
        if !migration_applied(&conn, 8)? {
            backfill_qa_findings(&conn)?;
            record_migration(&conn, 8, "v2_7_qa_findings")?;
        }
        // Version 10 (STORE-12) hardens `jobs.status` / `segments.status` with
        // CHECK constraints and doubles as the unknown-status warn-on-open
        // pass. Gated like 0009 because it is a one-time table rebuild.
        self.apply_status_check_constraints(&mut conn);

        Ok(())
    }
    /// STORE-12: enforce the canonical status vocabularies at the storage
    /// layer via CHECK constraints, tolerating pre-existing rows:
    ///
    /// - Fresh or already-conforming databases get the rebuild once; it is
    ///   recorded as migration 10 (`v2_8_status_check_constraints`) so later
    ///   opens skip both the scan and the warning pass entirely — from then on
    ///   the CHECKs guarantee no foreign status can be written again.
    /// - Legacy databases whose rows contain values outside the canonical
    ///   sets are left untouched (the original tables stay plain TEXT): the
    ///   open succeeds, a diagnostic warns about every unknown value until the
    ///   data is cleaned up, and reads decode such values defensively to the
    ///   explicit `Unknown(..)` variant. Old names/rows are never rewritten in
    ///   place silently.
    fn apply_status_check_constraints(&self, conn: &mut Connection) {
        let already_applied = match migration_applied(conn, 10) {
            Ok(applied) => applied,
            Err(error) => {
                self.push_diagnostic(format!(
                    "status check hardening skipped: could not read the migration ledger: {error}"
                ));
                return;
            }
        };
        if already_applied {
            return;
        }

        let offending_jobs =
            distinct_status_values_outside(conn, "jobs", JobStatus::KNOWN_DB_TEXTS);
        let offending_segments =
            distinct_status_values_outside(conn, "segments", SegmentStatus::KNOWN_DB_TEXTS);

        let (jobs_offenders, segments_offenders) = match (&offending_jobs, &offending_segments) {
            (Ok(jobs), Ok(segments)) => (jobs.clone(), segments.clone()),
            (Err(error), _) | (_, Err(error)) => {
                self.push_diagnostic(format!(
                    "status check hardening skipped: could not scan legacy status values: {error}"
                ));
                return;
            }
        };

        for (table, offenders) in [("jobs", &jobs_offenders), ("segments", &segments_offenders)] {
            for value in offenders {
                self.push_diagnostic(format!(
                    "warning: `{table}` contains non-canonical status {value:?}; \
                     treated as Unknown on read and excluded from storage-level enforcement"
                ));
            }
        }

        if !jobs_offenders.is_empty() || !segments_offenders.is_empty() {
            // Tolerate pre-existing rows: keep them exactly as they are rather
            // than failing the open or rewriting history. The migration stays
            // unapplied so a future open (after the data is corrected) can
            // still add the constraints.
            self.push_diagnostic(
                "status check constraints not applied: resolve the non-canonical \
                 statuses above to enable storage-level enforcement"
                    .to_string(),
            );
            return;
        }

        if let Err(error) = harden_status_tables_with_check_constraints(conn) {
            self.push_diagnostic(format!("status check hardening failed: {error}"));
            return;
        }
        if let Err(error) = record_migration(conn, 10, "v2_8_status_check_constraints") {
            self.push_diagnostic(format!(
                "status check hardening succeeded but could not record migration 10: {error}"
            ));
        }
    }
}

fn table_exists(conn: &Connection, table: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        params![table],
        |row| row.get::<_, bool>(0),
    )
}

fn table_has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for row in rows {
        if row? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn rename_table_if_exists(conn: &Connection, table: &str, suffix: u128) -> rusqlite::Result<()> {
    if table_exists(conn, table)? {
        conn.execute(
            &format!("ALTER TABLE {table} RENAME TO {table}_legacy_{suffix}"),
            [],
        )?;
    }
    Ok(())
}

fn ensure_column(
    conn: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> rusqlite::Result<()> {
    if !table_has_column(conn, table, column)? {
        conn.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
            [],
        )?;
    }
    Ok(())
}

fn ensure_glossary_target_nullable(conn: &Connection) -> rusqlite::Result<()> {
    if !table_exists(conn, "glossary_terms")?
        || !table_column_is_not_null(conn, "glossary_terms", "target_text")?
    {
        return Ok(());
    }

    // The rebuild (rename away → recreate → copy back → drop legacy) is one
    // transaction: a crash mid-rename previously orphaned the data and the
    // next open silently recreated an empty table.
    let tx = conn.unchecked_transaction()?;
    let legacy_table = format!("glossary_terms_v1_2_0_{}", unix_timestamp_nanos());
    tx.execute_batch(&format!(
        "
        DROP INDEX IF EXISTS idx_glossary_lookup;
        ALTER TABLE glossary_terms RENAME TO {legacy_table};
        CREATE TABLE glossary_terms (
          id INTEGER PRIMARY KEY,
          scope_kind TEXT NOT NULL CHECK(scope_kind IN ('global', 'series', 'book')),
          scope_id TEXT,
          source_text TEXT NOT NULL,
          target_text TEXT,
          category TEXT NOT NULL CHECK(category IN
            ('person', 'place', 'object', 'invented', 'style', 'phrase', 'other')),
          notes TEXT,
          case_sensitive INTEGER NOT NULL DEFAULT 0,
          always_active INTEGER NOT NULL DEFAULT 0,
          status TEXT NOT NULL CHECK(status IN
            ('user_seeded', 'auto_candidate', 'accepted', 'rejected'))
            DEFAULT 'user_seeded',
          source_language TEXT NOT NULL,
          target_language TEXT NOT NULL,
          source_count INTEGER DEFAULT 0,
          created_at TEXT NOT NULL,
          updated_at TEXT NOT NULL,
          UNIQUE(scope_kind, scope_id, source_text, source_language, target_language)
        );
        INSERT INTO glossary_terms
          (id, scope_kind, scope_id, source_text, target_text, category, notes,
           case_sensitive, always_active, status, source_language, target_language,
           source_count, created_at, updated_at)
        SELECT id, scope_kind, scope_id, source_text, target_text, category, notes,
               case_sensitive, always_active, status, source_language, target_language,
               source_count, created_at, updated_at
        FROM {legacy_table};
        DROP TABLE {legacy_table};
        ",
    ))?;
    tx.commit()?;
    Ok(())
}

pub(super) fn table_column_is_not_null(
    conn: &Connection,
    table: &str,
    column: &str,
) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            let not_null: i64 = row.get(3)?;
            return Ok(not_null != 0);
        }
    }
    Ok(false)
}

fn record_migration(conn: &Connection, version: i64, name: &str) -> rusqlite::Result<()> {
    // Gate the write behind an applied-check: connections that reopen the
    // store frequently (watchers, dashboards) must not take a write lock and
    // run a write transaction on every open just to re-record an already-
    // applied migration.
    if migration_applied(conn, version)? {
        return Ok(());
    }
    conn.execute(
        "INSERT INTO _migrations (version, name, applied_at)
         VALUES (?1, ?2, ?3)",
        params![version, name, timestamp_string()],
    )?;
    Ok(())
}

fn migration_applied(conn: &Connection, version: i64) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM _migrations WHERE version = ?1)",
        params![version],
        |row| row.get::<_, bool>(0),
    )
}

/// Distinct `status` values currently stored in `table` that are NOT part of
/// the canonical set enforced by the CHECK constraints. Defensive against
/// hand-edited legacy databases.
fn distinct_status_values_outside(
    conn: &Connection,
    table: &str,
    known: &[&str],
) -> rusqlite::Result<Vec<String>> {
    if !table_exists(conn, table)? {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(&format!("SELECT DISTINCT status FROM {table}"))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, Option<String>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    // Column is NOT NULL, but tolerate a hand-edited NULL as an offender too:
    // it is exactly what the rebuild must refuse to bless silently.
    Ok(rows
        .into_iter()
        .flatten()
        .filter(|value| !known.contains(&value.as_str()))
        .collect())
}

/// Column lists shared by the hardened CREATE TABLE and its data-copy SELECT,
/// so the rebuild can never silently drop or reorder what it preserves.
const JOB_COLUMNS_ALL: &[&str] = &[
    "id",
    "input_path",
    "input_snapshot_path",
    "input_sha256",
    "output_path",
    "input_hash",
    "source_lang",
    "target_lang",
    "provider",
    "model",
    "base_url",
    "api_key_env",
    "status",
    "config_json",
    "events_path",
    "report_json_path",
    "report_markdown_path",
    "book_id",
    "series_id",
    "created_at",
    "updated_at",
];

const SEGMENT_COLUMNS_ALL: &[&str] = &[
    "id",
    "job_id",
    "section_id",
    "ordinal",
    "source_hash",
    "prompt_version",
    "provider",
    "model",
    "status",
    "attempts",
    "input_tokens",
    "output_tokens",
    "tokens_input",
    "tokens_input_cached",
    "tokens_output",
    "tokens_estimated",
    "cost_estimate",
    "error",
    "translated_hash",
    "cache_namespace",
];

fn job_status_column_definition() -> String {
    format!(
        "status TEXT NOT NULL CHECK(status IN ({}))",
        job_sql_set(JobStatus::all_known())
    )
}

/// STORE-12 storage enforcement: one `IMMEDIATE` transaction rebuilds `jobs`
/// and `segments` with CHECK-constrained `status` columns, copying every row
/// verbatim, then recreates the indexes that lived on the dropped tables.
///
/// Follows the SQLite ALTER TABLE recipe: foreign_keys are disabled only
/// around the transaction (pragmas cannot change inside one), both parent
/// tables drop and rename inside the same transaction so a crash either keeps
/// the original tables untouched or leaves the fully-swapped, constrained
/// schema behind. `PRAGMA foreign_key_check` runs afterwards as a belt-and-
/// braces guard for anything the swap could have unlinked.
fn harden_status_tables_with_check_constraints(conn: &mut Connection) -> Result<()> {
    let jobs_status = job_status_column_definition();
    let segments_status = SegmentStatus::sql_set(SegmentStatus::all_known());
    let jobs_table_name = format!("jobs_hardened_{}", unix_timestamp_nanos());
    let segments_table_name = format!("segments_hardened_{}", unix_timestamp_nanos());

    let create_jobs = format!(
        "CREATE TABLE {jobs_table_name} (
          id TEXT PRIMARY KEY,
          input_path TEXT NOT NULL DEFAULT '',
          input_snapshot_path TEXT,
          input_sha256 TEXT,
          output_path TEXT NOT NULL DEFAULT '',
          input_hash TEXT NOT NULL,
          source_lang TEXT,
          target_lang TEXT NOT NULL,
          provider TEXT NOT NULL,
          model TEXT NOT NULL,
          base_url TEXT,
          api_key_env TEXT,
          {jobs_status},
          config_json TEXT,
          events_path TEXT,
          report_json_path TEXT,
          report_markdown_path TEXT,
          book_id TEXT,
          series_id TEXT,
          created_at TEXT NOT NULL,
          updated_at TEXT NOT NULL
        );"
    );
    let create_segments = format!(
        "CREATE TABLE {segments_table_name} (
          id TEXT NOT NULL,
          job_id TEXT NOT NULL,
          section_id TEXT NOT NULL,
          ordinal INTEGER NOT NULL,
          source_hash TEXT NOT NULL,
          prompt_version TEXT NOT NULL,
          provider TEXT NOT NULL,
          model TEXT NOT NULL,
          status TEXT NOT NULL CHECK(status IN ({segments_status})),
          attempts INTEGER NOT NULL DEFAULT 0,
          input_tokens INTEGER,
          output_tokens INTEGER,
          tokens_input INTEGER,
          tokens_input_cached INTEGER,
          tokens_output INTEGER,
          tokens_estimated INTEGER NOT NULL DEFAULT 0,
          cost_estimate REAL,
          error TEXT,
          translated_hash TEXT,
          cache_namespace TEXT NOT NULL DEFAULT '',
          PRIMARY KEY (job_id, id),
          FOREIGN KEY(job_id) REFERENCES jobs(id)
        );"
    );

    conn.pragma_update(None, "foreign_keys", false)?;
    harden_status_tables_inner(
        conn,
        &create_jobs,
        &create_segments,
        &jobs_table_name,
        &segments_table_name,
    )?;
    conn.pragma_update(None, "foreign_keys", true)?;

    // Data was only ever copied (not mutated), so by construction the swapped
    // schema has no dangling references. Still verify: any violation here
    // would mean the rebuild itself broke referential integrity.
    if let Some(offending) = first_foreign_key_violation(conn)? {
        return Err(StoreError::Serialization(format!(
            "status check hardening produced a foreign key violation involving table '{offending}'"
        )));
    }
    Ok(())
}

fn harden_status_tables_inner(
    conn: &mut Connection,
    create_jobs: &str,
    create_segments: &str,
    jobs_tmp: &str,
    segments_tmp: &str,
) -> rusqlite::Result<()> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute_batch(create_jobs)?;
    tx.execute_batch(create_segments)?;

    // Copy only the columns that exist in BOTH the hardened table and the
    // live source so partially-upgraded or very old databases never abort the
    // rebuild on a missing column (their hardened targets supply DEFAULTs).
    let jobs_source_columns = existing_columns(&tx, "jobs");
    let segments_source_columns = existing_columns(&tx, "segments");
    let jobs_source_columns = jobs_source_columns?;
    let segments_source_columns = segments_source_columns?;
    let jobs_copy_columns = JOB_COLUMNS_ALL
        .iter()
        .filter(|column| jobs_source_columns.contains::<str>(column))
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    let segments_copy_columns = SEGMENT_COLUMNS_ALL
        .iter()
        .filter(|column| segments_source_columns.contains::<str>(column))
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    tx.execute(
        &format!(
            "INSERT INTO {jobs_tmp} ({jobs_copy_columns})
             SELECT {jobs_copy_columns} FROM jobs"
        ),
        [],
    )?;
    tx.execute(
        &format!(
            "INSERT INTO {segments_tmp} ({segments_copy_columns})
             SELECT {segments_copy_columns} FROM segments"
        ),
        [],
    )?;

    // Child tables keep pointing at the same names; dropping the parents is
    // only legal while foreign_keys is off, which is exactly why it is toggled
    // around this transaction.
    tx.execute_batch(&format!(
        "DROP TABLE segments;
         DROP TABLE jobs;
         ALTER TABLE {segments_tmp} RENAME TO segments;
         ALTER TABLE {jobs_tmp} RENAME TO jobs;"
    ))?;
    // Indexes living on the dropped tables die with them; recreate exactly
    // the set the procedural migrator maintains on jobs/segments (IF NOT
    // EXISTS keeps this idempotent across retry attempts).
    tx.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_segments_cache_lookup
         ON segments(source_hash, cache_namespace, prompt_version, provider, model, status);
         CREATE INDEX IF NOT EXISTS idx_jobs_created_at
         ON jobs(created_at);",
    )?;
    tx.commit()?;
    Ok(())
}

fn first_foreign_key_violation(conn: &Connection) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT \"table\" FROM pragma_foreign_key_check LIMIT 1",
        [],
        |row| row.get::<_, String>(0),
    )
    .optional()
}

fn existing_columns(
    tx: &rusqlite::Transaction<'_>,
    table: &str,
) -> rusqlite::Result<std::collections::HashSet<String>> {
    let mut stmt = tx.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(names.into_iter().collect())
}

/// Remove duplicate global-scope rows that the NULL-tolerant table UNIQUE
/// constraints allowed to accumulate, keeping the most recently updated row
/// per identity (ties broken by higher id = later insert). Runs before the
/// partial unique indexes are created so index construction cannot fail on
/// legacy data.
fn deduplicate_global_scope_rows(conn: &Connection) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "DELETE FROM glossary_terms AS kept
          WHERE kept.scope_kind = 'global'
            AND EXISTS (
              SELECT 1 FROM glossary_terms AS newer
               WHERE newer.scope_kind = 'global'
                 AND newer.source_text = kept.source_text
                 AND newer.source_language = kept.source_language
                 AND newer.target_language = kept.target_language
                 AND (newer.updated_at, newer.id) > (kept.updated_at, kept.id)
            )",
        [],
    )?;
    tx.execute(
        "DELETE FROM style_sheets AS kept
          WHERE kept.scope_kind = 'global'
            AND EXISTS (
              SELECT 1 FROM style_sheets AS newer
               WHERE newer.scope_kind = 'global'
                 AND newer.target_language = kept.target_language
                 AND (newer.updated_at, newer.id) > (kept.updated_at, kept.id)
            )",
        [],
    )?;
    tx.execute(
        "DELETE FROM entities AS kept
          WHERE kept.scope_kind = 'global'
            AND EXISTS (
              SELECT 1 FROM entities AS newer
               WHERE newer.scope_kind = 'global'
                 AND newer.source_name = kept.source_name
                 AND newer.source_language = kept.source_language
                 AND newer.target_language = kept.target_language
                 AND (newer.updated_at, newer.id) > (kept.updated_at, kept.id)
            )",
        [],
    )?;
    tx.commit()?;
    Ok(())
}

fn backfill_qa_findings(conn: &Connection) -> rusqlite::Result<usize> {
    let flagged_statuses =
        SegmentStatus::sql_set(&[SegmentStatus::NeedsReview, SegmentStatus::Failed]);
    let rows = {
        let sql = format!(
            "SELECT s.job_id, s.id, s.error
             FROM segments s
             WHERE s.status IN ({flagged_statuses})
               AND s.error IS NOT NULL
               AND TRIM(s.error) <> ''
               AND NOT EXISTS (
                 SELECT 1 FROM qa_findings f
                 WHERE f.job_id = s.job_id AND f.segment_id = s.id
               )
             ORDER BY s.job_id, s.id"
        );
        let mut stmt = conn.prepare(&sql)?;
        stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?
    };

    let mut inserted = 0usize;
    for (job_id, segment_id, error) in rows {
        for (index, finding) in classify_segment_error(&error).into_iter().enumerate() {
            let hash = stable_hash(&format!("{job_id}\u{1f}{segment_id}\u{1f}{index}"));
            let id = format!("qaf_{}", &hash[..24]);
            inserted += conn.execute(
                "INSERT OR REPLACE INTO qa_findings
                 (id, segment_id, job_id, severity, kind, message)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    id,
                    segment_id,
                    job_id,
                    finding.severity.as_str(),
                    finding.kind.as_str(),
                    finding.message,
                ],
            )?;
        }
    }
    Ok(inserted)
}

#[cfg(all(test, unix))]
mod unix_permissions_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn open_creates_private_bookforge_directory() {
        let root = std::env::temp_dir().join(format!(
            "bookforge-store-permissions-{}-{}",
            std::process::id(),
            unix_timestamp_nanos()
        ));
        fs::create_dir(&root).expect("test root should be created");
        let bookforge_dir = root.join(".bookforge");
        let db_path = bookforge_dir.join("jobs.sqlite");

        let store = JobStore::open(&db_path).expect("store should open");
        let mode = fs::metadata(&bookforge_dir)
            .expect(".bookforge metadata should be readable")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);

        drop(store);
        fs::remove_dir_all(&root).expect("test directory should be removed");
    }
}
