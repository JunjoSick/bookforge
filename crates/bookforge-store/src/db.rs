use std::{
    cell::RefCell,
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bookforge_core::{
    Result as CoreResult,
    ir::BlockId,
    run_snapshot::RunConfigSnapshot,
    segment::{BlockTranslation, Segment},
};
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

    #[error("serialization error: {0}")]
    Serialization(String),
}

pub struct JobStore {
    conn: RefCell<Connection>,
    path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct JobRecord {
    pub id: String,
    pub input_path: PathBuf,
    pub output_path: PathBuf,
    pub input_hash: String,
    pub source_lang: Option<String>,
    pub target_lang: String,
    pub provider: String,
    pub model: String,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub status: String,
    pub events_path: Option<PathBuf>,
    pub report_json_path: Option<PathBuf>,
    pub report_markdown_path: Option<PathBuf>,
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
    pub cached: usize,
    pub retried: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct CreateJob<'a> {
    pub input: &'a Path,
    pub output: &'a Path,
    pub source_lang: Option<&'a str>,
    pub target_lang: &'a str,
    pub provider: &'a str,
    pub model: &'a str,
    pub base_url: Option<&'a str>,
    pub api_key_env: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct SegmentRecord {
    pub id: String,
    pub status: String,
    pub attempts: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredBlockTranslation {
    pub segment_id: String,
    pub block_id: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSegmentTranslation {
    pub segment_id: String,
    pub ordinal: usize,
    pub status: String,
    pub error: Option<String>,
    pub translated_text: String,
    pub blocks: Vec<BlockTranslation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedTranslation {
    pub translated_text: String,
    pub blocks: Vec<BlockTranslation>,
}

#[derive(Debug, Clone, Copy)]
pub struct SaveTranslation<'a> {
    pub job_id: &'a str,
    pub segment_id: &'a str,
    pub translated_text: &'a str,
    pub blocks: &'a [BlockTranslation],
    pub provider: &'a str,
    pub model: &'a str,
    pub prompt_version: &'a str,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
pub struct SaveNeedsReview<'a> {
    pub job_id: &'a str,
    pub segment_id: &'a str,
    pub preserved_text: &'a str,
    pub blocks: &'a [BlockTranslation],
    pub provider: &'a str,
    pub model: &'a str,
    pub prompt_version: &'a str,
    pub error: &'a str,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
pub struct SaveCachedTranslation<'a> {
    pub job_id: &'a str,
    pub segment_id: &'a str,
    pub translated_text: &'a str,
    pub blocks: &'a [BlockTranslation],
    pub provider: &'a str,
    pub model: &'a str,
    pub prompt_version: &'a str,
}

#[derive(Debug, Clone, Copy)]
pub struct CacheLookupRequest<'a> {
    pub prompt_version: &'a str,
    pub provider: &'a str,
    pub model: &'a str,
    pub source_lang: Option<&'a str>,
    pub target_lang: &'a str,
    pub cache_namespace: &'a str,
}

#[derive(Debug, Clone)]
pub struct StorageDoctor {
    pub database_path: PathBuf,
    pub database_exists: bool,
    pub wal_present: bool,
    pub shm_present: bool,
    pub journal_mode: String,
    pub integrity_check: String,
    pub wal_sidecars_normal: bool,
    pub note: String,
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
            fs::create_dir_all(parent)?;
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

    pub fn create_job(&self, request: CreateJob<'_>) -> Result<JobRecord> {
        let input_hash = file_hash(request.input)?;
        let id = format!("job_{}_{}", unix_timestamp_nanos(), &input_hash[..12]);
        let now = timestamp_string();
        let input_path = request.input.to_path_buf();
        let output_path = request.output.to_path_buf();
        let conn = self.conn.borrow();
        conn.execute(
            "INSERT INTO jobs
             (id, input_path, output_path, input_hash, source_lang, target_lang, provider, model, base_url, api_key_env, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'running', ?11, ?11)",
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
                now,
            ],
        )?;

        Ok(JobRecord {
            id,
            input_path,
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
                 updated_at = ?5
             WHERE id = ?6",
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
                timestamp_string(),
                job_id,
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
        for segment in segments {
            tx.execute(
                "INSERT OR IGNORE INTO segments
                 (id, job_id, section_id, ordinal, source_hash, prompt_version, provider, model, status, attempts, cache_namespace)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'queued', 0, ?9)",
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
        let now = timestamp_string();
        let translated_hash = stable_hash(request.translated_text);
        {
            let conn = self.conn.borrow();
            conn.execute(
                "INSERT OR REPLACE INTO translations
                 (segment_id, job_id, translated_text, provider, model, prompt_version, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
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
            replace_block_translations(&conn, request.job_id, request.segment_id, request.blocks)?;
            conn.execute(
                "UPDATE segments
                 SET status = 'succeeded', attempts = attempts + 1, input_tokens = ?1, output_tokens = ?2, translated_hash = ?3, error = NULL
                 WHERE job_id = ?4 AND id = ?5",
                params![
                    request.input_tokens.map(|value| value as i64),
                    request.output_tokens.map(|value| value as i64),
                    translated_hash,
                    request.job_id,
                    request.segment_id,
                ],
            )?;
        }
        self.touch_job(request.job_id, "running")?;
        Ok(())
    }

    pub fn save_needs_review(&self, request: SaveNeedsReview<'_>) -> Result<()> {
        let now = timestamp_string();
        let translated_hash = stable_hash(request.preserved_text);
        {
            let conn = self.conn.borrow();
            conn.execute(
                "INSERT OR REPLACE INTO translations
                 (segment_id, job_id, translated_text, provider, model, prompt_version, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
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
            replace_block_translations(&conn, request.job_id, request.segment_id, request.blocks)?;
            conn.execute(
                "UPDATE segments
                 SET status = 'needs_review', attempts = attempts + 1, input_tokens = ?1, output_tokens = ?2, translated_hash = ?3, error = ?4
                 WHERE job_id = ?5 AND id = ?6",
                params![
                    request.input_tokens.map(|value| value as i64),
                    request.output_tokens.map(|value| value as i64),
                    translated_hash,
                    request.error,
                    request.job_id,
                    request.segment_id
                ],
            )?;
        }
        self.touch_job(request.job_id, "needs_review")?;
        Ok(())
    }

    pub fn save_cached_translation(&self, request: SaveCachedTranslation<'_>) -> Result<()> {
        let now = timestamp_string();
        let translated_hash = stable_hash(request.translated_text);
        {
            let conn = self.conn.borrow();
            conn.execute(
                "INSERT OR REPLACE INTO translations
                 (segment_id, job_id, translated_text, provider, model, prompt_version, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
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
            replace_block_translations(&conn, request.job_id, request.segment_id, request.blocks)?;
            conn.execute(
                "UPDATE segments
                 SET status = 'skipped_cached', input_tokens = NULL, output_tokens = NULL, translated_hash = ?1, error = NULL
                 WHERE job_id = ?2 AND id = ?3",
                params![translated_hash, request.job_id, request.segment_id],
            )?;
        }
        self.touch_job(request.job_id, "running")?;
        Ok(())
    }

    pub fn mark_job_complete(&self, job_id: &str) -> Result<()> {
        self.touch_job(job_id, "succeeded")
    }

    pub fn mark_job_running(&self, job_id: &str) -> Result<()> {
        self.touch_job(job_id, "running")
    }

    pub fn mark_job_succeeded(&self, job_id: &str) -> Result<()> {
        self.mark_job_complete(job_id)
    }

    pub fn mark_job_needs_review(&self, job_id: &str) -> Result<()> {
        self.touch_job(job_id, "needs_review")
    }

    pub fn mark_job_interrupted(&self, job_id: &str) -> Result<()> {
        self.touch_job(job_id, "interrupted")
    }

    pub fn mark_job_failed(&self, job_id: &str) -> Result<()> {
        self.touch_job(job_id, "failed")
    }

    pub fn mark_segment_failed(&self, job_id: &str, segment_id: &str, error: &str) -> Result<()> {
        {
            let conn = self.conn.borrow();
            conn.execute(
                "UPDATE segments SET status = 'failed', attempts = attempts + 1, error = ?1 WHERE job_id = ?2 AND id = ?3",
                params![error, job_id, segment_id],
            )?;
        }
        self.touch_job(job_id, "failed")?;
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
        self.touch_job(job_id, "failed")?;
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
            self.touch_job(job_id, "failed")?;
        }
        Ok(updated)
    }

    pub fn get_job(&self, job_id: &str) -> Result<Option<JobRecord>> {
        let conn = self.conn.borrow();
        conn.query_row(
            "SELECT id, input_path, output_path, input_hash, source_lang, target_lang, provider, model, base_url, api_key_env, status,
                    events_path, report_json_path, report_markdown_path
             FROM jobs WHERE id = ?1",
            params![job_id],
            |row| {
                Ok(JobRecord {
                    id: row.get(0)?,
                    input_path: PathBuf::from(row.get::<_, String>(1)?),
                    output_path: PathBuf::from(row.get::<_, String>(2)?),
                    input_hash: row.get(3)?,
                    source_lang: row.get(4)?,
                    target_lang: row.get(5)?,
                    provider: row.get(6)?,
                    model: row.get(7)?,
                    base_url: row.get(8)?,
                    api_key_env: row.get(9)?,
                    status: row.get(10)?,
                    events_path: row.get::<_, Option<String>>(11)?.map(PathBuf::from),
                    report_json_path: row.get::<_, Option<String>>(12)?.map(PathBuf::from),
                    report_markdown_path: row.get::<_, Option<String>>(13)?.map(PathBuf::from),
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
        let conn = self.conn.borrow();
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
        self.touch_job(job_id, "retry_pending")?;
        Ok(count)
    }

    pub fn pending_segment_ids(&self, job_id: &str) -> Result<Vec<String>> {
        let conn = self.conn.borrow();
        let mut stmt = conn.prepare(
            "SELECT id FROM segments
             WHERE job_id = ?1 AND status IN ('queued', 'retry_pending')
             ORDER BY ordinal",
        )?;
        let rows = stmt.query_map(params![job_id], |row| row.get::<_, String>(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn segment_records(&self, job_id: &str) -> Result<Vec<SegmentRecord>> {
        let conn = self.conn.borrow();
        let mut stmt = conn.prepare(
            "SELECT id, status, attempts, error FROM segments WHERE job_id = ?1 ORDER BY ordinal",
        )?;
        let rows = stmt.query_map(params![job_id], |row| {
            Ok(SegmentRecord {
                id: row.get(0)?,
                status: row.get(1)?,
                attempts: row.get::<_, i64>(2)? as usize,
                error: row.get(3)?,
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
        let mut stmt = conn.prepare(
            "SELECT s.id, s.ordinal, s.status, s.error, t.translated_text
             FROM segments s
             JOIN translations t ON t.job_id = s.job_id AND t.segment_id = s.id
             WHERE s.job_id = ?1 AND s.status IN ('succeeded', 'skipped_cached', 'needs_review')
             ORDER BY s.ordinal",
        )?;
        let rows = stmt.query_map(params![job_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;

        let mut records = Vec::new();
        for row in rows {
            let (segment_id, ordinal, status, error, translated_text) = row?;
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
            });
        }

        Ok(records)
    }

    pub fn resumable_segment_ids(&self, job_id: &str) -> Result<Vec<String>> {
        let conn = self.conn.borrow();
        let mut stmt = conn.prepare(
            "SELECT id FROM segments
             WHERE job_id = ?1 AND status IN ('queued', 'retry_pending', 'failed')
             ORDER BY ordinal",
        )?;
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
        let cached = conn
            .query_row(
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
                   AND s.status IN ('succeeded', 'skipped_cached')
                 ORDER BY t.created_at DESC
                 LIMIT 1",
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
                   AND s.status IN ('succeeded', 'skipped_cached')
                 ORDER BY t.created_at DESC",
                hashes.len() + 1,
                hashes.len() + 2,
                hashes.len() + 3,
                hashes.len() + 4,
                hashes.len() + 5,
                hashes.len() + 6,
                hashes.len() + 7,
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
            CREATE TABLE IF NOT EXISTS jobs (
              id TEXT PRIMARY KEY,
              input_path TEXT NOT NULL DEFAULT '',
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
            ",
        )?;
        ensure_column(&conn, "jobs", "input_path", "TEXT NOT NULL DEFAULT ''")?;
        ensure_column(&conn, "jobs", "output_path", "TEXT NOT NULL DEFAULT ''")?;
        ensure_column(&conn, "jobs", "base_url", "TEXT")?;
        ensure_column(&conn, "jobs", "api_key_env", "TEXT")?;
        ensure_column(&conn, "jobs", "config_json", "TEXT")?;
        ensure_column(&conn, "jobs", "events_path", "TEXT")?;
        ensure_column(&conn, "jobs", "report_json_path", "TEXT")?;
        ensure_column(&conn, "jobs", "report_markdown_path", "TEXT")?;
        ensure_column(
            &conn,
            "segments",
            "cache_namespace",
            "TEXT NOT NULL DEFAULT ''",
        )?;
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_segments_cache_lookup
             ON segments(source_hash, cache_namespace, prompt_version, provider, model, status);",
        )?;
        Ok(())
    }

    fn touch_job(&self, job_id: &str, status: &str) -> Result<()> {
        let conn = self.conn.borrow();
        conn.execute(
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

#[cfg(test)]
mod tests {
    use super::*;
    use bookforge_core::{
        ir::{BlockId, SectionId},
        segment::{
            Segment, SegmentBlock, SegmentConstraints, SegmentContext, SegmentId, SegmentMetadata,
            SegmentSource, SegmentTextRun,
        },
    };

    #[test]
    fn store_reuses_connection_across_job_operations() {
        let db_path = temp_path("jobs.sqlite");
        let input_path = temp_path("input.epub");
        fs::write(&input_path, b"epub bytes").expect("input fixture should be writable");

        let store = JobStore::open(&db_path).expect("store should open");
        let job = store
            .create_job(CreateJob {
                input: &input_path,
                output: &temp_path("output.epub"),
                source_lang: Some("English"),
                target_lang: "Italian",
                provider: "mock",
                model: "mock-prefix",
                base_url: None,
                api_key_env: None,
            })
            .expect("job should be created");
        let segments = vec![segment("seg_a", 0), segment("seg_b", 1)];
        store
            .insert_segments(&job.id, &segments, "v1", "mock", "mock-prefix", "test_ns")
            .expect("segments should insert");

        store
            .save_translation(SaveTranslation {
                job_id: &job.id,
                segment_id: "seg_a",
                translated_text: "Tradotto",
                blocks: &[BlockTranslation {
                    block_id: BlockId("b_000000".to_string()),
                    text: "Tradotto".to_string(),
                }],
                provider: "mock",
                model: "mock-prefix",
                prompt_version: "v1",
                input_tokens: Some(11),
                output_tokens: Some(7),
            })
            .expect("translation should save");
        store
            .mark_segment_failed(&job.id, "seg_b", "provider unavailable")
            .expect("segment should be marked failed");

        let summary = store
            .summary(&job.id)
            .expect("summary should load")
            .expect("job should exist");
        assert_eq!(summary.total_segments, 2);
        assert_eq!(summary.succeeded, 1);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.input_tokens, 11);
        assert_eq!(summary.output_tokens, 7);
        let blocks = store
            .load_block_translations(&job.id)
            .expect("block translations should load");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text, "Tradotto");

        let _ = fs::remove_file(db_path);
        let _ = fs::remove_file(input_path);
    }

    fn segment(id: &str, ordinal: usize) -> Segment {
        let block_id = BlockId(format!("b_{ordinal:06}"));
        Segment {
            id: SegmentId(id.to_string()),
            section_id: SectionId("sec_000000".to_string()),
            ordinal,
            block_ids: vec![block_id.clone()],
            source: SegmentSource {
                text: format!("Source {ordinal}"),
                blocks: vec![SegmentBlock {
                    block_id,
                    kind: "paragraph".to_string(),
                    text: format!("Source {ordinal}"),
                    text_runs: vec![SegmentTextRun {
                        id: format!("r{ordinal}"),
                        text: format!("Source {ordinal}"),
                    }],
                    protected_spans: Vec::new(),
                }],
                token_estimate: 2,
            },
            context: SegmentContext::default(),
            metadata: SegmentMetadata::default(),
            constraints: SegmentConstraints::default(),
            checksum: format!("checksum_{ordinal}"),
        }
    }

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "bookforge-store-test-{}-{}-{name}",
            std::process::id(),
            unix_timestamp_nanos()
        ))
    }

    fn build_seeded_store_with_translation(
        db_path: &PathBuf,
        cache_namespace: &str,
        block_ids: &[&str],
    ) -> (JobStore, JobRecord, Segment) {
        let input_path = temp_path("input.epub");
        fs::write(&input_path, b"epub bytes").expect("input fixture should be writable");

        let store = JobStore::open(db_path).expect("store should open");
        let job = store
            .create_job(CreateJob {
                input: &input_path,
                output: &temp_path("output.epub"),
                source_lang: Some("English"),
                target_lang: "Italian",
                provider: "mock",
                model: "mock-prefix",
                base_url: None,
                api_key_env: None,
            })
            .expect("job should be created");

        let mut seg = segment("seg_a", 0);
        let blocks: Vec<BlockTranslation> = block_ids
            .iter()
            .map(|id| BlockTranslation {
                block_id: BlockId(id.to_string()),
                text: format!("Tradotto {id}"),
            })
            .collect();
        seg.block_ids = block_ids.iter().map(|id| BlockId(id.to_string())).collect();

        store
            .insert_segments(
                &job.id,
                &[seg.clone()],
                "v1",
                "mock",
                "mock-prefix",
                cache_namespace,
            )
            .expect("segments should insert");
        store
            .save_translation(SaveTranslation {
                job_id: &job.id,
                segment_id: "seg_a",
                translated_text: "Tradotto",
                blocks: &blocks,
                provider: "mock",
                model: "mock-prefix",
                prompt_version: "v1",
                input_tokens: Some(11),
                output_tokens: Some(7),
            })
            .expect("translation should save");

        let _ = fs::remove_file(input_path);
        (store, job, seg)
    }

    #[test]
    fn job_config_snapshot_round_trips_through_store() {
        let db_path = temp_path("snapshot.sqlite");
        let input_path = temp_path("input.epub");
        fs::write(&input_path, b"epub bytes").expect("input fixture should be writable");
        let store = JobStore::open(&db_path).expect("store should open");
        let job = store
            .create_job(CreateJob {
                input: &input_path,
                output: &temp_path("output.epub"),
                source_lang: Some("English"),
                target_lang: "Italian",
                provider: "openrouter",
                model: "model",
                base_url: Some("https://example.test/v1"),
                api_key_env: Some("OPENROUTER_API_KEY"),
            })
            .expect("job should be created");
        let settings = bookforge_core::TranslationProfile::Balanced.resolve();
        let snapshot = RunConfigSnapshot {
            input_path: input_path.clone(),
            output_path: temp_path("translated.epub"),
            events_path: Some(temp_path("events.jsonl")),
            report_json_path: Some(temp_path("report.json")),
            report_markdown_path: Some(temp_path("report.md")),
            source_language: Some("English".to_string()),
            target_language: "Italian".to_string(),
            provider: "openrouter".to_string(),
            model: "model".to_string(),
            base_url: Some("https://example.test/v1".to_string()),
            api_key_env: Some("OPENROUTER_API_KEY".to_string()),
            profile: settings.profile,
            provider_preset: None,
            prompt_version: "batch_v1".to_string(),
            cache_namespace: "cache_ns".to_string(),
            settings: bookforge_core::ResolvedRunSettingsSnapshot::from_settings(&settings),
        };

        store
            .update_job_config_snapshot(&job.id, &snapshot)
            .expect("snapshot should persist");
        let loaded = store
            .load_job_config_snapshot(&job.id)
            .expect("snapshot should load")
            .expect("snapshot should exist");
        assert_eq!(loaded, snapshot);

        let reloaded_job = store
            .get_job(&job.id)
            .expect("job should load")
            .expect("job should exist");
        assert_eq!(reloaded_job.events_path, snapshot.events_path);
        assert_eq!(reloaded_job.report_json_path, snapshot.report_json_path);
        assert_eq!(
            reloaded_job.report_markdown_path,
            snapshot.report_markdown_path
        );

        let _ = fs::remove_file(db_path);
        let _ = fs::remove_file(input_path);
    }

    #[test]
    fn job_config_snapshot_does_not_store_api_key_value() {
        let db_path = temp_path("snapshot_secret.sqlite");
        let input_path = temp_path("input.epub");
        fs::write(&input_path, b"epub bytes").expect("input fixture should be writable");
        let api_key_env = "BOOKFORGE_TEST_API_KEY_VALUE_NOT_STORED";
        let api_key_value = "sk-live-secret-that-must-not-be-persisted";
        // This test uses a unique process-local env var and verifies snapshot
        // serialization never reads the value.
        unsafe {
            std::env::set_var(api_key_env, api_key_value);
        }

        let store = JobStore::open(&db_path).expect("store should open");
        let job = store
            .create_job(CreateJob {
                input: &input_path,
                output: &temp_path("output.epub"),
                source_lang: Some("English"),
                target_lang: "Italian",
                provider: "openrouter",
                model: "model",
                base_url: Some("https://example.test/v1"),
                api_key_env: Some(api_key_env),
            })
            .expect("job should be created");
        let settings = bookforge_core::TranslationProfile::Balanced.resolve();
        let snapshot = RunConfigSnapshot {
            input_path: input_path.clone(),
            output_path: temp_path("translated.epub"),
            events_path: Some(temp_path("events.jsonl")),
            report_json_path: Some(temp_path("report.json")),
            report_markdown_path: Some(temp_path("report.md")),
            source_language: Some("English".to_string()),
            target_language: "Italian".to_string(),
            provider: "openrouter".to_string(),
            model: "model".to_string(),
            base_url: Some("https://example.test/v1".to_string()),
            api_key_env: Some(api_key_env.to_string()),
            profile: settings.profile,
            provider_preset: None,
            prompt_version: "batch_v1".to_string(),
            cache_namespace: "cache_ns".to_string(),
            settings: bookforge_core::ResolvedRunSettingsSnapshot::from_settings(&settings),
        };

        store
            .update_job_config_snapshot(&job.id, &snapshot)
            .expect("snapshot should persist");
        let raw_json = {
            let conn = store.conn.borrow();
            conn.query_row(
                "SELECT config_json FROM jobs WHERE id = ?1",
                params![job.id],
                |row| row.get::<_, String>(0),
            )
            .expect("raw snapshot JSON should load")
        };

        assert!(raw_json.contains(api_key_env));
        assert!(!raw_json.contains(api_key_value));

        unsafe {
            std::env::remove_var(api_key_env);
        }
        let _ = fs::remove_file(db_path);
        let _ = fs::remove_file(input_path);
    }

    #[test]
    fn terminal_loading_and_resumable_ids_preserve_lifecycle_boundaries() {
        let db_path = temp_path("terminal_resume.sqlite");
        let input_path = temp_path("input.epub");
        fs::write(&input_path, b"epub bytes").expect("input fixture should be writable");
        let store = JobStore::open(&db_path).expect("store should open");
        let job = store
            .create_job(CreateJob {
                input: &input_path,
                output: &temp_path("output.epub"),
                source_lang: Some("English"),
                target_lang: "Italian",
                provider: "mock",
                model: "mock-prefix",
                base_url: None,
                api_key_env: None,
            })
            .expect("job should be created");
        let segments = vec![
            segment("seg_done", 0),
            segment("seg_cached", 1),
            segment("seg_review", 2),
            segment("seg_failed", 3),
            segment("seg_queued", 4),
        ];
        store
            .insert_segments(&job.id, &segments, "v1", "mock", "mock-prefix", "ns")
            .expect("segments should insert");
        store
            .save_translation(SaveTranslation {
                job_id: &job.id,
                segment_id: "seg_done",
                translated_text: "Done",
                blocks: &[BlockTranslation {
                    block_id: BlockId("b_000000".to_string()),
                    text: "Done".to_string(),
                }],
                provider: "mock",
                model: "mock-prefix",
                prompt_version: "v1",
                input_tokens: None,
                output_tokens: None,
            })
            .expect("done should save");
        store
            .save_cached_translation(SaveCachedTranslation {
                job_id: &job.id,
                segment_id: "seg_cached",
                translated_text: "Cached",
                blocks: &[BlockTranslation {
                    block_id: BlockId("b_000001".to_string()),
                    text: "Cached".to_string(),
                }],
                provider: "mock",
                model: "mock-prefix",
                prompt_version: "v1",
            })
            .expect("cached should save");
        store
            .save_needs_review(SaveNeedsReview {
                job_id: &job.id,
                segment_id: "seg_review",
                preserved_text: "Review",
                blocks: &[BlockTranslation {
                    block_id: BlockId("b_000002".to_string()),
                    text: "Review".to_string(),
                }],
                provider: "mock",
                model: "mock-prefix",
                prompt_version: "v1",
                error: "needs eyes",
                input_tokens: None,
                output_tokens: None,
            })
            .expect("review should save");
        store
            .mark_segment_failed(&job.id, "seg_failed", "failed")
            .expect("failed should mark");

        let terminal = store
            .load_terminal_segment_translations(&job.id)
            .expect("terminal records should load");
        let ids = terminal
            .iter()
            .map(|record| record.segment_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["seg_done", "seg_cached", "seg_review"]);
        assert_eq!(terminal[0].blocks[0].block_id.0, "b_000000");
        assert_eq!(terminal[2].status, "needs_review");

        let resumable = store
            .resumable_segment_ids(&job.id)
            .expect("resumable ids should load");
        assert_eq!(resumable, vec!["seg_failed", "seg_queued"]);

        let _ = fs::remove_file(db_path);
        let _ = fs::remove_file(input_path);
    }

    #[test]
    fn mark_unfinished_segments_failed_preserves_terminal_segments() {
        let db_path = temp_path("unfinished_preserve.sqlite");
        let input_path = temp_path("input.epub");
        fs::write(&input_path, b"epub bytes").expect("input fixture should be writable");
        let store = JobStore::open(&db_path).expect("store should open");
        let job = store
            .create_job(CreateJob {
                input: &input_path,
                output: &temp_path("output.epub"),
                source_lang: Some("English"),
                target_lang: "Italian",
                provider: "mock",
                model: "mock-prefix",
                base_url: None,
                api_key_env: None,
            })
            .expect("job should be created");
        let segments = vec![
            segment("seg_succeeded", 0),
            segment("seg_cached", 1),
            segment("seg_review", 2),
            segment("seg_queued", 3),
            segment("seg_retry", 4),
        ];
        store
            .insert_segments(&job.id, &segments, "v1", "mock", "mock-prefix", "test_ns")
            .expect("segments should insert");
        store
            .save_translation(SaveTranslation {
                job_id: &job.id,
                segment_id: "seg_succeeded",
                translated_text: "Done",
                blocks: &[BlockTranslation {
                    block_id: BlockId("b_000000".to_string()),
                    text: "Done".to_string(),
                }],
                provider: "mock",
                model: "mock-prefix",
                prompt_version: "v1",
                input_tokens: None,
                output_tokens: None,
            })
            .expect("succeeded segment should save");
        store
            .save_cached_translation(SaveCachedTranslation {
                job_id: &job.id,
                segment_id: "seg_cached",
                translated_text: "Cached",
                blocks: &[BlockTranslation {
                    block_id: BlockId("b_000001".to_string()),
                    text: "Cached".to_string(),
                }],
                provider: "mock",
                model: "mock-prefix",
                prompt_version: "v1",
            })
            .expect("cached segment should save");
        store
            .save_needs_review(SaveNeedsReview {
                job_id: &job.id,
                segment_id: "seg_review",
                preserved_text: "Needs review",
                blocks: &[BlockTranslation {
                    block_id: BlockId("b_000002".to_string()),
                    text: "Needs review".to_string(),
                }],
                provider: "mock",
                model: "mock-prefix",
                prompt_version: "v1",
                error: "qa issue",
                input_tokens: None,
                output_tokens: None,
            })
            .expect("needs-review segment should save");
        store
            .retry_segments(&job.id, RetryScope::Failed)
            .expect("retry with no failed segments should be harmless");
        {
            let conn = store.conn.borrow();
            conn.execute(
                "UPDATE segments SET status = 'retry_pending' WHERE job_id = ?1 AND id = 'seg_retry'",
                params![job.id],
            )
            .expect("test status update should work");
        }

        let candidate_ids = segments
            .iter()
            .map(|segment| segment.id.0.clone())
            .collect::<Vec<_>>();
        let changed = store
            .mark_unfinished_segments_failed(&job.id, &candidate_ids, "run failed")
            .expect("unfinished segments should be marked failed");
        assert_eq!(changed, 2);

        let records = store.segment_records(&job.id).expect("records should load");
        let statuses = records
            .into_iter()
            .map(|record| (record.id, record.status))
            .collect::<HashMap<_, _>>();
        assert_eq!(statuses["seg_succeeded"], "succeeded");
        assert_eq!(statuses["seg_cached"], "skipped_cached");
        assert_eq!(statuses["seg_review"], "needs_review");
        assert_eq!(statuses["seg_queued"], "failed");
        assert_eq!(statuses["seg_retry"], "failed");

        let _ = fs::remove_file(db_path);
        let _ = fs::remove_file(input_path);
    }

    #[test]
    fn mark_unfinished_segments_failed_marks_only_resumable_segments() {
        let db_path = temp_path("unfinished_resumable_only.sqlite");
        let input_path = temp_path("input.epub");
        fs::write(&input_path, b"epub bytes").expect("input fixture should be writable");
        let store = JobStore::open(&db_path).expect("store should open");
        let job = store
            .create_job(CreateJob {
                input: &input_path,
                output: &temp_path("output.epub"),
                source_lang: Some("English"),
                target_lang: "Italian",
                provider: "mock",
                model: "mock-prefix",
                base_url: None,
                api_key_env: None,
            })
            .expect("job should be created");
        let segments = vec![
            segment("seg_succeeded", 0),
            segment("seg_cached", 1),
            segment("seg_review", 2),
            segment("seg_failed", 3),
            segment("seg_retry", 4),
            segment("seg_queued", 5),
        ];
        store
            .insert_segments(&job.id, &segments, "v1", "mock", "mock-prefix", "test_ns")
            .expect("segments should insert");
        {
            let conn = store.conn.borrow();
            for (id, status) in [
                ("seg_succeeded", "succeeded"),
                ("seg_cached", "skipped_cached"),
                ("seg_review", "needs_review"),
                ("seg_failed", "failed"),
                ("seg_retry", "retry_pending"),
                ("seg_queued", "queued"),
            ] {
                conn.execute(
                    "UPDATE segments SET status = ?1 WHERE job_id = ?2 AND id = ?3",
                    params![status, job.id, id],
                )
                .expect("status should update");
            }
        }

        let candidate_ids = segments
            .iter()
            .map(|segment| segment.id.0.clone())
            .collect::<Vec<_>>();
        let changed = store
            .mark_unfinished_segments_failed(&job.id, &candidate_ids, "run failed")
            .expect("unfinished segments should be marked failed");

        assert_eq!(changed, 3);
        let records = store.segment_records(&job.id).expect("records should load");
        let statuses = records
            .into_iter()
            .map(|record| (record.id, record.status))
            .collect::<HashMap<_, _>>();
        assert_eq!(statuses["seg_succeeded"], "succeeded");
        assert_eq!(statuses["seg_cached"], "skipped_cached");
        assert_eq!(statuses["seg_review"], "needs_review");
        assert_eq!(statuses["seg_failed"], "failed");
        assert_eq!(statuses["seg_retry"], "failed");
        assert_eq!(statuses["seg_queued"], "failed");

        let _ = fs::remove_file(db_path);
        let _ = fs::remove_file(input_path);
    }

    #[test]
    fn resumable_segment_ids_excludes_succeeded_cached_and_needs_review() {
        let db_path = temp_path("resumable_excludes.sqlite");
        let input_path = temp_path("input.epub");
        fs::write(&input_path, b"epub bytes").expect("input fixture should be writable");
        let store = JobStore::open(&db_path).expect("store should open");
        let job = store
            .create_job(CreateJob {
                input: &input_path,
                output: &temp_path("output.epub"),
                source_lang: Some("English"),
                target_lang: "Italian",
                provider: "mock",
                model: "mock-prefix",
                base_url: None,
                api_key_env: None,
            })
            .expect("job should be created");
        let segments = vec![
            segment("seg_succeeded", 0),
            segment("seg_cached", 1),
            segment("seg_review", 2),
        ];
        store
            .insert_segments(&job.id, &segments, "v1", "mock", "mock-prefix", "test_ns")
            .expect("segments should insert");
        {
            let conn = store.conn.borrow();
            for (id, status) in [
                ("seg_succeeded", "succeeded"),
                ("seg_cached", "skipped_cached"),
                ("seg_review", "needs_review"),
            ] {
                conn.execute(
                    "UPDATE segments SET status = ?1 WHERE job_id = ?2 AND id = ?3",
                    params![status, job.id, id],
                )
                .expect("status should update");
            }
        }

        let ids = store
            .resumable_segment_ids(&job.id)
            .expect("resumable ids should load");

        assert!(ids.is_empty());
        let _ = fs::remove_file(db_path);
        let _ = fs::remove_file(input_path);
    }

    #[test]
    fn resumable_segment_ids_includes_failed_retry_pending_and_pending() {
        let db_path = temp_path("resumable_includes.sqlite");
        let input_path = temp_path("input.epub");
        fs::write(&input_path, b"epub bytes").expect("input fixture should be writable");
        let store = JobStore::open(&db_path).expect("store should open");
        let job = store
            .create_job(CreateJob {
                input: &input_path,
                output: &temp_path("output.epub"),
                source_lang: Some("English"),
                target_lang: "Italian",
                provider: "mock",
                model: "mock-prefix",
                base_url: None,
                api_key_env: None,
            })
            .expect("job should be created");
        let segments = vec![
            segment("seg_failed", 0),
            segment("seg_retry", 1),
            segment("seg_queued", 2),
        ];
        store
            .insert_segments(&job.id, &segments, "v1", "mock", "mock-prefix", "test_ns")
            .expect("segments should insert");
        {
            let conn = store.conn.borrow();
            for (id, status) in [
                ("seg_failed", "failed"),
                ("seg_retry", "retry_pending"),
                ("seg_queued", "queued"),
            ] {
                conn.execute(
                    "UPDATE segments SET status = ?1 WHERE job_id = ?2 AND id = ?3",
                    params![status, job.id, id],
                )
                .expect("status should update");
            }
        }

        let ids = store
            .resumable_segment_ids(&job.id)
            .expect("resumable ids should load");

        assert_eq!(ids, vec!["seg_failed", "seg_retry", "seg_queued"]);
        let _ = fs::remove_file(db_path);
        let _ = fs::remove_file(input_path);
    }

    #[test]
    fn cached_translation_requires_matching_cache_namespace() {
        let db_path = temp_path("ns_match.sqlite");
        let (store, _job, seg) =
            build_seeded_store_with_translation(&db_path, "ns_one", &["b_000000"]);

        let hit = store
            .find_cached_translation(
                &seg,
                "v1",
                "mock",
                "mock-prefix",
                Some("English"),
                "Italian",
                "ns_one",
            )
            .expect("query ok");
        assert!(hit.is_some(), "matching namespace should hit");

        let miss = store
            .find_cached_translation(
                &seg,
                "v1",
                "mock",
                "mock-prefix",
                Some("English"),
                "Italian",
                "ns_two",
            )
            .expect("query ok");
        assert!(miss.is_none(), "different namespace must not hit");

        let _ = fs::remove_file(db_path);
    }

    #[test]
    fn cached_translation_rejects_mismatched_block_ids() {
        let db_path = temp_path("blockid_match.sqlite");
        let (store, _job, mut seg) =
            build_seeded_store_with_translation(&db_path, "ns_x", &["b_000000"]);

        // Caller's segment now expects different block IDs than what was stored.
        seg.block_ids = vec![BlockId("b_999999".to_string())];

        let miss = store
            .find_cached_translation(
                &seg,
                "v1",
                "mock",
                "mock-prefix",
                Some("English"),
                "Italian",
                "ns_x",
            )
            .expect("query ok");
        assert!(
            miss.is_none(),
            "mismatched block_ids must reject the cached row"
        );

        let _ = fs::remove_file(db_path);
    }

    #[test]
    fn old_empty_cache_namespace_rows_do_not_match_new_runs() {
        let db_path = temp_path("legacy_ns.sqlite");
        // Simulate a row migrated from an older schema with the default empty namespace.
        let (store, _job, seg) = build_seeded_store_with_translation(&db_path, "", &["b_000000"]);

        let miss = store
            .find_cached_translation(
                &seg,
                "v1",
                "mock",
                "mock-prefix",
                Some("English"),
                "Italian",
                "real_ns",
            )
            .expect("query ok");
        assert!(
            miss.is_none(),
            "legacy empty-namespace row must not satisfy a real namespace lookup"
        );

        let _ = fs::remove_file(db_path);
    }

    #[test]
    fn job_store_enables_wal_and_busy_timeout() {
        let db_path = temp_path("wal_busy.sqlite");
        let store = JobStore::open(&db_path).expect("store should open");

        let conn = store.conn.borrow();
        let journal_mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .expect("pragma journal_mode should succeed");
        assert_eq!(
            journal_mode.to_lowercase(),
            "wal",
            "WAL journal mode must be enabled"
        );

        let busy_timeout: i64 = conn
            .pragma_query_value(None, "busy_timeout", |row| row.get(0))
            .expect("pragma busy_timeout should succeed");
        assert!(
            busy_timeout >= 5000,
            "busy_timeout should be at least 5000ms, got {busy_timeout}"
        );

        let wal_path = db_path.with_extension("sqlite-wal");
        let shm_path = db_path.with_extension("sqlite-shm");
        // WAL/shm may or may not exist depending on transactions, but the
        // journal_mode query confirms WAL is active.

        let _ = fs::remove_file(db_path);
        let _ = fs::remove_file(wal_path);
        let _ = fs::remove_file(shm_path);
    }

    #[test]
    fn job_store_enables_foreign_keys_on_every_connection() {
        let db_path = temp_path("fk.sqlite");
        let store = JobStore::open(&db_path).expect("store should open");

        let conn = store.conn.borrow();
        let fk_enabled: i64 = conn
            .pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .expect("pragma foreign_keys should succeed");
        assert_eq!(
            fk_enabled, 1,
            "foreign_keys pragma must be ON on every connection"
        );

        let _ = fs::remove_file(db_path);
    }

    #[test]
    fn doctor_reports_wal_sidecars_as_normal_when_integrity_check_passes() {
        let db_path = temp_path("doctor.sqlite");
        let store = JobStore::open(&db_path).expect("store should open");

        // Perform a write to trigger WAL sidecar creation.
        let input_path = temp_path("input_doctor.epub");
        fs::write(&input_path, b"epub bytes").expect("test epub");
        let _job = store
            .create_job(CreateJob {
                input: &input_path,
                output: &temp_path("out_doctor.epub"),
                source_lang: Some("English"),
                target_lang: "Italian",
                provider: "mock",
                model: "mock-prefix",
                base_url: None,
                api_key_env: None,
            })
            .expect("job created");
        drop(store);

        let doctor = run_doctor(Some(db_path.clone())).expect("doctor should run");
        assert!(doctor.database_exists, "database should exist");
        assert_eq!(
            doctor.journal_mode.to_lowercase(),
            "wal",
            "journal mode should be wal"
        );
        assert_eq!(doctor.integrity_check, "ok", "integrity check should pass");
        assert!(
            doctor.wal_sidecars_normal,
            "wal sidecars should be reported as normal"
        );

        if doctor.wal_present || doctor.shm_present {
            assert!(
                !doctor.note.is_empty(),
                "doctor must explain WAL sidecars when they are present"
            );
        }

        let wal_path = db_path.with_extension("sqlite-wal");
        let shm_path = db_path.with_extension("sqlite-shm");
        let _ = fs::remove_file(&db_path);
        let _ = fs::remove_file(input_path);
        let _ = fs::remove_file(wal_path);
        let _ = fs::remove_file(shm_path);
    }

    #[test]
    fn checkpoint_writer_and_reader_do_not_immediately_busy_fail() {
        let db_path = temp_path("concurrent.sqlite");
        let input_path = temp_path("input_conc.epub");
        fs::write(&input_path, b"epub bytes").expect("test epub");

        // Open writer store first and create a job.
        let store_w = JobStore::open(&db_path).expect("store_w open");
        let job = store_w
            .create_job(CreateJob {
                input: &input_path,
                output: &temp_path("out_conc.epub"),
                source_lang: Some("English"),
                target_lang: "Italian",
                provider: "mock",
                model: "mock-prefix",
                base_url: None,
                api_key_env: None,
            })
            .expect("job created");
        store_w
            .insert_segments(
                &job.id,
                &[segment("seg_conc", 0)],
                "v1",
                "mock",
                "mock-prefix",
                "ns",
            )
            .expect("segments inserted");

        // Open a second reader store while the first is still active.
        let store_r = JobStore::open(&db_path).expect("store_r open");
        let summary = store_r
            .summary(&job.id)
            .expect("summary should load")
            .expect("job should exist");
        assert_eq!(summary.total_segments, 1);

        let wal_path = db_path.with_extension("sqlite-wal");
        let shm_path = db_path.with_extension("sqlite-shm");
        let _ = fs::remove_file(&db_path);
        let _ = fs::remove_file(input_path);
        let _ = fs::remove_file(wal_path);
        let _ = fs::remove_file(shm_path);
    }
}
