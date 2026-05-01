use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use bookforge_core::{Result as CoreResult, segment::Segment};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

pub type Result<T> = std::result::Result<T, StoreError>;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("core error: {0}")]
    Core(#[from] bookforge_core::BookforgeError),
}

#[derive(Debug, Clone)]
pub struct JobStore {
    path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct JobRecord {
    pub id: String,
    pub input_hash: String,
    pub source_lang: Option<String>,
    pub target_lang: String,
    pub provider: String,
    pub model: String,
    pub status: String,
}

#[derive(Debug, Clone, Default)]
pub struct JobSummary {
    pub id: String,
    pub status: String,
    pub total_segments: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub needs_review: usize,
    pub retry_pending: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl JobStore {
    pub fn open_default() -> Result<Self> {
        Self::open(".bookforge/jobs.sqlite")
    }

    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let store = Self { path };
        store.migrate()?;
        Ok(store)
    }

    pub fn create_job(
        &self,
        input: &Path,
        source_lang: Option<&str>,
        target_lang: &str,
        provider: &str,
        model: &str,
    ) -> Result<JobRecord> {
        let input_hash = file_hash(input)?;
        let id = format!("job_{}_{}", unix_timestamp_nanos(), &input_hash[..12]);
        let now = timestamp_string();
        let conn = self.connect()?;
        conn.execute(
            "INSERT INTO jobs (id, input_hash, source_lang, target_lang, provider, model, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'running', ?7, ?7)",
            params![id, input_hash, source_lang, target_lang, provider, model, now],
        )?;

        Ok(JobRecord {
            id,
            input_hash,
            source_lang: source_lang.map(ToOwned::to_owned),
            target_lang: target_lang.to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            status: "running".to_string(),
        })
    }

    pub fn insert_segments(
        &self,
        job_id: &str,
        segments: &[Segment],
        prompt_version: &str,
        provider: &str,
        model: &str,
    ) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        for segment in segments {
            tx.execute(
                "INSERT OR IGNORE INTO segments
                 (id, job_id, section_id, ordinal, source_hash, prompt_version, provider, model, status, attempts)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'queued', 0)",
                params![
                    segment.id.0,
                    job_id,
                    segment.section_id.0,
                    segment.ordinal as i64,
                    segment.checksum,
                    prompt_version,
                    provider,
                    model,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn save_translation(
        &self,
        job_id: &str,
        segment_id: &str,
        translated_text: &str,
        provider: &str,
        model: &str,
        prompt_version: &str,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
    ) -> Result<()> {
        let now = timestamp_string();
        let translated_hash = stable_hash(translated_text);
        let conn = self.connect()?;
        conn.execute(
            "INSERT OR REPLACE INTO translations
             (segment_id, job_id, translated_text, provider, model, prompt_version, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                segment_id,
                job_id,
                translated_text,
                provider,
                model,
                prompt_version,
                now
            ],
        )?;
        conn.execute(
            "UPDATE segments
             SET status = 'succeeded', attempts = attempts + 1, input_tokens = ?1, output_tokens = ?2, translated_hash = ?3, error = NULL
             WHERE job_id = ?4 AND id = ?5",
            params![
                input_tokens.map(|value| value as i64),
                output_tokens.map(|value| value as i64),
                translated_hash,
                job_id,
                segment_id,
            ],
        )?;
        self.touch_job(job_id, "running")?;
        Ok(())
    }

    pub fn mark_job_complete(&self, job_id: &str) -> Result<()> {
        self.touch_job(job_id, "succeeded")
    }

    pub fn mark_segment_failed(&self, job_id: &str, segment_id: &str, error: &str) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE segments SET status = 'failed', attempts = attempts + 1, error = ?1 WHERE job_id = ?2 AND id = ?3",
            params![error, job_id, segment_id],
        )?;
        self.touch_job(job_id, "failed")?;
        Ok(())
    }

    pub fn get_job(&self, job_id: &str) -> Result<Option<JobRecord>> {
        self.connect()?
            .query_row(
                "SELECT id, input_hash, source_lang, target_lang, provider, model, status FROM jobs WHERE id = ?1",
                params![job_id],
                |row| {
                    Ok(JobRecord {
                        id: row.get(0)?,
                        input_hash: row.get(1)?,
                        source_lang: row.get(2)?,
                        target_lang: row.get(3)?,
                        provider: row.get(4)?,
                        model: row.get(5)?,
                        status: row.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn summary(&self, job_id: &str) -> Result<Option<JobSummary>> {
        let Some(job) = self.get_job(job_id)? else {
            return Ok(None);
        };
        let conn = self.connect()?;
        let mut summary = JobSummary {
            id: job.id,
            status: job.status,
            ..JobSummary::default()
        };

        let mut stmt = conn.prepare(
            "SELECT status, COUNT(*), COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0)
             FROM segments WHERE job_id = ?1 GROUP BY status",
        )?;
        let rows = stmt.query_map(params![job_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;

        for row in rows {
            let (status, count, input_tokens, output_tokens) = row?;
            let count = count as usize;
            summary.total_segments += count;
            summary.input_tokens += input_tokens as u64;
            summary.output_tokens += output_tokens as u64;
            match status.as_str() {
                "succeeded" => summary.succeeded += count,
                "failed" => summary.failed += count,
                "needs_review" => summary.needs_review += count,
                "retry_pending" => summary.retry_pending += count,
                _ => {}
            }
        }

        Ok(Some(summary))
    }

    pub fn retry_segments(&self, job_id: &str, scope: RetryScope) -> Result<usize> {
        let where_status = match scope {
            RetryScope::Failed => "status = 'failed'",
            RetryScope::NeedsReview => "status = 'needs_review'",
            RetryScope::All => "status IN ('failed', 'needs_review')",
        };
        let sql = format!(
            "UPDATE segments SET status = 'retry_pending', error = NULL WHERE job_id = ?1 AND {where_status}"
        );
        let count = self.connect()?.execute(&sql, params![job_id])?;
        self.touch_job(job_id, "retry_pending")?;
        Ok(count)
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.connect()?;
        if table_exists(&conn, "translations")?
            && !table_has_column(&conn, "translations", "job_id")?
        {
            conn.execute_batch(
                "
                DROP TABLE IF EXISTS qa_findings;
                DROP TABLE IF EXISTS translations;
                DROP TABLE IF EXISTS segments;
                DROP TABLE IF EXISTS jobs;
                ",
            )?;
        }
        conn.execute_batch(
            "
            PRAGMA foreign_keys = ON;
            CREATE TABLE IF NOT EXISTS jobs (
              id TEXT PRIMARY KEY,
              input_hash TEXT NOT NULL,
              source_lang TEXT,
              target_lang TEXT NOT NULL,
              provider TEXT NOT NULL,
              model TEXT NOT NULL,
              status TEXT NOT NULL,
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
              PRIMARY KEY (job_id, segment_id),
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
            ",
        )?;
        Ok(())
    }

    fn connect(&self) -> rusqlite::Result<Connection> {
        Connection::open(&self.path)
    }

    fn touch_job(&self, job_id: &str, status: &str) -> Result<()> {
        self.connect()?.execute(
            "UPDATE jobs SET status = ?1, updated_at = ?2 WHERE id = ?3",
            params![status, timestamp_string(), job_id],
        )?;
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

#[derive(Debug, Clone, Copy)]
pub enum RetryScope {
    Failed,
    NeedsReview,
    All,
}

fn file_hash(path: &Path) -> CoreResult<String> {
    let bytes = fs::read(path)?;
    Ok(stable_hash_bytes(&bytes))
}

fn stable_hash(text: &str) -> String {
    stable_hash_bytes(text.as_bytes())
}

fn stable_hash_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to string should not fail");
    }
    output
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn unix_timestamp_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn timestamp_string() -> String {
    unix_timestamp().to_string()
}
