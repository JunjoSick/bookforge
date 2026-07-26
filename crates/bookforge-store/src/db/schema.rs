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

        let store = Self {
            conn: RefCell::new(conn),
            path,
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.conn.borrow();
        if table_exists(&conn, "translations")?
            && !table_has_column(&conn, "translations", "job_id")?
        {
            let suffix = unix_timestamp_nanos();
            rename_table_if_exists(&conn, "qa_findings", suffix)?;
            rename_table_if_exists(&conn, "translation_blocks", suffix)?;
            rename_table_if_exists(&conn, "translations", suffix)?;
            rename_table_if_exists(&conn, "segments", suffix)?;
            rename_table_if_exists(&conn, "jobs", suffix)?;
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
             ON qa_findings(job_id, segment_id);",
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
        Ok(())
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

    let legacy_table = format!("glossary_terms_v1_2_0_{}", unix_timestamp_nanos());
    conn.execute_batch(&format!(
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
    conn.execute(
        "INSERT OR IGNORE INTO _migrations (version, name, applied_at)
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

fn backfill_qa_findings(conn: &Connection) -> rusqlite::Result<usize> {
    let rows = {
        let mut stmt = conn.prepare(
            "SELECT s.job_id, s.id, s.error
             FROM segments s
             WHERE s.status IN ('needs_review', 'failed')
               AND s.error IS NOT NULL
               AND TRIM(s.error) <> ''
               AND NOT EXISTS (
                 SELECT 1 FROM qa_findings f
                 WHERE f.job_id = s.job_id AND f.segment_id = s.id
               )
             ORDER BY s.job_id, s.id",
        )?;
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
