use std::{
    cell::RefCell,
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bookforge_core::{
    Result as CoreResult,
    entity::EntityGender,
    glossary::{GlossaryCategory, GlossaryScopeKind, GlossaryStatus, GlossaryTerm},
    ir::BlockId,
    run_snapshot::RunConfigSnapshot,
    segment::{BlockTranslation, Segment},
};
use rusqlite::{Connection, OptionalExtension, params, types::Type};
use sha2::{Digest, Sha256};
use std::str::FromStr;

mod flags;
mod glossary;

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

    #[error("manual correction rejected: {0}")]
    InvalidCorrection(String),
}

pub struct JobStore {
    conn: RefCell<Connection>,
    path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct JobRecord {
    pub id: String,
    pub input_path: PathBuf,
    pub input_snapshot_path: Option<PathBuf>,
    pub input_sha256: Option<String>,
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
    pub book_id: Option<String>,
    pub series_id: Option<String>,
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
    pub input_cached_tokens: u64,
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
    pub book_id: Option<&'a str>,
    pub series_id: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct SegmentRecord {
    pub id: String,
    pub status: String,
    pub attempts: usize,
    pub error: Option<String>,
    pub input_tokens: Option<u64>,
    pub input_cached_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub tokens_estimated: bool,
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
    pub provider: String,
    pub model: String,
    pub human_corrected: bool,
    pub corrected_at: Option<String>,
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
    pub input_cached_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub tokens_estimated: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct SaveManualCorrection<'a> {
    pub job_id: &'a str,
    pub segment_id: &'a str,
    pub translated_text: &'a str,
    pub blocks: &'a [BlockTranslation],
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
    pub input_cached_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub tokens_estimated: bool,
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

#[derive(Debug, Clone, Copy)]
pub struct NewSegmentFlag<'a> {
    pub job_id: &'a str,
    pub segment_id: &'a str,
    pub kind: &'a str,
    pub note: Option<&'a str>,
    pub suggested_source: Option<&'a str>,
    pub suggested_target: Option<&'a str>,
    pub consumed: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct GlossaryFilter<'a> {
    pub scope_kind: Option<GlossaryScopeKind>,
    pub scope_id: Option<&'a str>,
    pub source_language: Option<&'a str>,
    pub target_language: Option<&'a str>,
    pub active_only: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct NewGlossaryCandidate<'a> {
    pub source_text: &'a str,
    pub category: GlossaryCategory,
    pub source_count: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GlossaryCandidateUpsertResult {
    pub inserted: usize,
    pub updated: usize,
    pub skipped: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredGlossaryCandidate {
    pub id: i64,
    pub source_text: String,
    pub target_text: Option<String>,
    pub category: GlossaryCategory,
    pub notes: Option<String>,
    pub case_sensitive: bool,
    pub always_active: bool,
    pub status: GlossaryStatus,
    pub source_language: String,
    pub target_language: String,
    pub source_count: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct NewStyleSheet<'a> {
    pub scope_kind: GlossaryScopeKind,
    pub scope_id: Option<&'a str>,
    pub target_language: &'a str,
    pub content_toml: &'a str,
    pub fingerprint: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredStyleSheet {
    pub id: i64,
    pub scope_kind: GlossaryScopeKind,
    pub scope_id: Option<String>,
    pub target_language: String,
    pub content_toml: String,
    pub fingerprint: String,
}

#[derive(Debug, Clone, Copy)]
pub struct NewEntity<'a> {
    pub scope_kind: GlossaryScopeKind,
    pub scope_id: Option<&'a str>,
    pub source_name: &'a str,
    pub target_name: &'a str,
    pub gender_target: Option<EntityGender>,
    pub role: Option<&'a str>,
    pub notes: Option<&'a str>,
    pub source_language: &'a str,
    pub target_language: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredEntity {
    pub id: i64,
    pub scope_kind: GlossaryScopeKind,
    pub scope_id: Option<String>,
    pub source_name: String,
    pub target_name: String,
    pub gender_target: Option<EntityGender>,
    pub role: Option<String>,
    pub notes: Option<String>,
    pub source_language: String,
    pub target_language: String,
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
        if self.translation_is_human_corrected(request.job_id, request.segment_id)? {
            return Ok(());
        }
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
                 SET status = 'succeeded',
                     attempts = attempts + 1,
                     tokens_input = ?1,
                     tokens_input_cached = ?2,
                     tokens_output = ?3,
                     tokens_estimated = ?4,
                     translated_hash = ?5,
                     error = NULL
                 WHERE job_id = ?6 AND id = ?7",
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
        }
        self.consume_dashboard_retry_guidance(request.job_id, request.segment_id)?;
        self.touch_job_unless_status(request.job_id, "running", &["paused", "stopped"])?;
        Ok(())
    }

    pub fn save_needs_review(&self, request: SaveNeedsReview<'_>) -> Result<()> {
        if self.translation_is_human_corrected(request.job_id, request.segment_id)? {
            return Ok(());
        }
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
                 SET status = 'needs_review',
                     attempts = attempts + 1,
                     tokens_input = ?1,
                     tokens_input_cached = ?2,
                     tokens_output = ?3,
                     tokens_estimated = ?4,
                     translated_hash = ?5,
                     error = ?6
                 WHERE job_id = ?7 AND id = ?8",
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
        }
        self.consume_dashboard_retry_guidance(request.job_id, request.segment_id)?;
        self.touch_job_unless_status(request.job_id, "needs_review", &["paused", "stopped"])?;
        Ok(())
    }

    pub fn save_cached_translation(&self, request: SaveCachedTranslation<'_>) -> Result<()> {
        if self.translation_is_human_corrected(request.job_id, request.segment_id)? {
            return Ok(());
        }
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
                 SET status = 'skipped_cached',
                     tokens_input = NULL,
                     tokens_input_cached = NULL,
                     tokens_output = NULL,
                     tokens_estimated = 0,
                     translated_hash = ?1,
                     error = NULL
                 WHERE job_id = ?2 AND id = ?3",
                params![translated_hash, request.job_id, request.segment_id],
            )?;
        }
        self.consume_dashboard_retry_guidance(request.job_id, request.segment_id)?;
        self.touch_job_unless_status(request.job_id, "running", &["paused", "stopped"])?;
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
        let tx = conn.transaction()?;
        let job_status = tx
            .query_row(
                "SELECT status FROM jobs WHERE id = ?1",
                params![request.job_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(job_status) = job_status else {
            return Err(StoreError::InvalidCorrection(format!(
                "job '{}' was not found",
                request.job_id
            )));
        };
        if matches!(job_status.as_str(), "running" | "paused") {
            return Err(StoreError::InvalidCorrection(format!(
                "job '{}' is {}; stop it before applying a manual correction",
                request.job_id, job_status
            )));
        }

        let prompt_version = tx
            .query_row(
                "SELECT prompt_version FROM segments WHERE job_id = ?1 AND id = ?2",
                params![request.job_id, request.segment_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(prompt_version) = prompt_version else {
            return Err(StoreError::InvalidCorrection(format!(
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
            "UPDATE segments
             SET status = 'succeeded', translated_hash = ?1, error = NULL
             WHERE job_id = ?2 AND id = ?3",
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
        Ok(conn
            .query_row(
                "SELECT human_corrected FROM translations WHERE job_id = ?1 AND segment_id = ?2",
                params![job_id, segment_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .is_some_and(|value| value != 0))
    }

    fn consume_dashboard_retry_guidance(&self, job_id: &str, segment_id: &str) -> Result<()> {
        let conn = self.conn.borrow();
        conn.execute(
            "UPDATE segment_flags SET consumed = 1
             WHERE job_id = ?1 AND segment_id = ?2 AND kind = 'dashboard_retry'",
            params![job_id, segment_id],
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
        let mut stmt = conn.prepare(
            "SELECT s.id, s.ordinal, s.status, s.error, t.translated_text,
                    t.provider, t.model, t.human_corrected, t.corrected_at
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
                   AND t.human_corrected = 0
                 ORDER BY CASE s.status WHEN 'succeeded' THEN 0 ELSE 1 END,
                          CAST(t.created_at AS INTEGER) DESC,
                          t.rowid DESC
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
                   AND t.human_corrected = 0
                 ORDER BY CASE s.status WHEN 'succeeded' THEN 0 ELSE 1 END,
                          CAST(t.created_at AS INTEGER) DESC,
                          t.rowid DESC",
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
             ON entities(source_language, target_language, scope_kind, scope_id);",
        )?;
        record_migration(&conn, 1, "initial")?;
        record_migration(&conn, 2, "v1_0_1_input_snapshot")?;
        record_migration(&conn, 3, "v1_1_segment_flags")?;
        record_migration(&conn, 4, "v1_2_glossary_terms")?;
        record_migration(&conn, 5, "v1_2_1_nullable_glossary_candidate_targets")?;
        record_migration(&conn, 6, "v1_3_context_styles_entities")?;
        record_migration(&conn, 7, "v2_4_human_corrections")?;
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

    fn touch_job_unless_status(
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

fn table_column_is_not_null(
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
mod tests;
