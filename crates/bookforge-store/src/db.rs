use std::{
    cell::RefCell,
    collections::HashMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Mutex,
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

mod assets_delete;
mod findings;
mod flags;
mod glossary;
mod jobs;
mod prune;
mod schema;
mod status;
mod translations;

#[cfg(test)]
mod migrations_docs;

// Internal cross-module helpers shared by the transactional checkpoint paths.
use findings::{NewQaFindingRow, insert_qa_finding_row};
use jobs::{ensure_job_exists, touch_job_unless_status_on};
use translations::{
    clear_segment_findings_on, record_segment_engine_findings_on, record_segment_findings_on,
};

pub use findings::{
    EngineFinding, QaFinding, QaFindingCount, QaFindingKind, QaFindingSeverity, StoredQaFinding,
    aggregate_findings, classify_segment_error,
};
pub use flags::RetryScope;
pub use prune::{PruneJobDeletion, PruneJobsOptions, PruneJobsReport};
pub use schema::run_doctor;
#[cfg(test)]
use schema::{restore_safe_status_harden, table_column_is_not_null};
pub use status::{JobStatus, SegmentStatus};

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

    #[error("not found: {0}")]
    NotFound(String),
}

pub struct JobStore {
    conn: RefCell<Connection>,
    path: PathBuf,
    /// Non-fatal conditions noticed while opening or migrating (for example
    /// status values outside the canonical sets on legacy databases). Empty in
    /// the common case; `take_diagnostics` drains them for surfacing.
    diagnostics: Mutex<Vec<String>>,
}

impl JobStore {
    pub(crate) fn new(conn: Connection, path: PathBuf) -> Self {
        Self {
            conn: RefCell::new(conn),
            path,
            diagnostics: Mutex::new(Vec::new()),
        }
    }

    /// Drain non-fatal open/migration diagnostics (warn-once per call). The
    /// common case is an empty vec: every field is only populated when the
    /// underlying database carries state BookForge never wrote.
    pub fn take_diagnostics(&self) -> Vec<String> {
        self.diagnostics
            .lock()
            .map(|mut guard| std::mem::take(&mut *guard))
            .unwrap_or_default()
    }

    pub(crate) fn push_diagnostic(&self, message: String) {
        if let Ok(mut guard) = self.diagnostics.lock() {
            guard.push(message);
        }
    }
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

impl JobRecord {
    /// Typed decode of [`Self::status`]. Unknown legacy values decode to
    /// [`JobStatus::Unknown`] instead of panicking.
    pub fn job_status(&self) -> JobStatus {
        JobStatus::from_db_text(&self.status)
    }
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

impl JobSummary {
    /// Typed decode of [`Self::status`]. See [`JobRecord::job_status`].
    pub fn job_status(&self) -> JobStatus {
        JobStatus::from_db_text(&self.status)
    }
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

impl SegmentRecord {
    /// Typed decode of [`Self::status`]. Unknown legacy values decode to
    /// [`SegmentStatus::Unknown`] instead of panicking.
    pub fn segment_status(&self) -> SegmentStatus {
        SegmentStatus::from_db_text(&self.status)
    }
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

impl StoredSegmentTranslation {
    /// Typed decode of [`Self::status`]. See [`SegmentRecord::segment_status`].
    pub fn segment_status(&self) -> SegmentStatus {
        SegmentStatus::from_db_text(&self.status)
    }
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

/// SHA-256 of a file read in bounded chunks (STORE-17 part B). EPUB inputs run
/// into the hundreds of megabytes; hashing streamed 64 KiB at a time keeps the
/// store's peak memory flat instead of loading the whole file into RAM like the
/// former `fs::read` + one-shot digest did.
const FILE_HASH_CHUNK_BYTES: usize = 64 * 1024;

fn file_hash(path: &Path) -> CoreResult<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut chunk = vec![0u8; FILE_HASH_CHUNK_BYTES];
    loop {
        let read = file.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        hasher.update(&chunk[..read]);
    }
    let digest = hasher.finalize();
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
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
