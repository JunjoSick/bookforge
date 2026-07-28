//! `bookforge serve` — a local web dashboard for monitoring (and retrying) jobs.
//!
//! This is the non-developer half of the v2 UI: the same [`bookforge_core::RunState`]
//! that powers the terminal `watch` dashboard is served over HTTP so a translator
//! friend can watch a run from a browser (or from a laptop over an SSH tunnel)
//! without opening a terminal.
//!
//! Design (see `docs/v2-web-dashboard-plan.md`):
//! - **axum** behind the default-on `serve` feature.
//! - **Server-side fold:** the server tails `events.jsonl`, folds into `RunState`,
//!   and pushes the snapshot as JSON — no fold logic duplicated in the browser.
//! - **SSE** (one-way server→client) for live updates; no WebSocket.
//! - Frontend is inline string consts (no build step), mirroring `review.rs`.
//! - Binds `127.0.0.1` by default; the book text is private.

use std::{
    collections::{BTreeMap, HashMap},
    convert::Infallible,
    ffi::OsString,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::{Command, ExitStatus},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Multipart, Path as AxumPath, Query, Request, State},
    http::{HeaderMap, StatusCode, header::HOST},
    middleware::{self, Next},
    response::{
        Html, IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{delete, get, post},
};
use bookforge_core::{
    ControlCommand, GlossaryCategory, GlossaryScopeKind, GlossaryStatus, GlossaryTerm, RunState,
    now_ms,
};
use bookforge_store::{GlossaryFilter, JobRecord, JobStore, JobSummary, RetryScope};
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;

use crate::eventlog::{EventLogTailer, events_path_for};

mod assets;
mod audio;
mod glossary;
mod jobs;
mod options;
mod security;
mod translation;

use super::{audiobook, correct, estimate, reconfigure, review, validate};
use security::{
    generate_csrf_token, reject_mutation, require_loopback_bind, validate_dashboard_host,
};
use translation::{
    configure_dashboard_child_environment, provider_key_env, resolve_dashboard_provider_key,
};

/// Where browser-launched uploads and their outputs are written.
const UPLOAD_DIR: &str = ".bookforge/serve-uploads";

/// Cap on a multipart upload body (EPUBs in the regression corpus reach ~11 MB).
const MAX_UPLOAD_BYTES: usize = 64 * 1024 * 1024;

/// Briefly check a detached translation child before reporting launch success.
const CHILD_STARTUP_CHECK: Duration = Duration::from_millis(150);

const ELEVENLABS_BASE_URL: &str = "https://api.elevenlabs.io/v1";
const ELEVENLABS_VOICE_CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const ELEVENLABS_VOICE_TIMEOUT_SECONDS: u64 = 10;

/// Cloud providers the dashboard form offers, paired with the env var their
/// runs read a key from when one is configured in the operator's environment.
const PROVIDER_KEY_ENVS: &[(&str, &str)] = &[
    ("deepseek", "DEEPSEEK_API_KEY"),
    ("openrouter", "OPENROUTER_API_KEY"),
    ("openai-compatible", "OPENAI_API_KEY"),
];
const AUDIO_PROVIDER_KEY_ENVS: &[(&str, &str)] = &[
    ("openai", "OPENAI_API_KEY"),
    ("gemini", "GEMINI_API_KEY"),
    ("elevenlabs", "ELEVENLABS_API_KEY"),
];
const AUDIO_OPENAI_MODELS: &[&str] = &["gpt-4o-mini-tts", "tts-1", "tts-1-hd"];
const AUDIO_GEMINI_MODELS: &[&str] = &["gemini-3.1-flash-tts-preview"];
const AUDIO_ELEVENLABS_MODELS: &[&str] = &[
    "eleven_v3",
    "eleven_flash_v2_5",
    "eleven_turbo_v2_5",
    "eleven_multilingual_v2",
];
const AUDIO_MOCK_MODELS: &[&str] = &["mock-silence"];

const LANGUAGE_OPTIONS: &[&str] = &[
    "English",
    "Italian",
    "Spanish",
    "French",
    "German",
    "Portuguese",
    "Dutch",
    "Polish",
    "Romanian",
    "Russian",
    "Ukrainian",
    "Turkish",
    "Japanese",
    "Korean",
    "Chinese (Simplified)",
    "Chinese (Traditional)",
    "Arabic",
    "Hindi",
    "Greek",
    "Swedish",
    "Norwegian",
    "Danish",
    "Finnish",
    "Czech",
    "Hungarian",
    "Indonesian",
    "Vietnamese",
    "Thai",
    "Toki Pona",
];

const MOCK_MODELS: &[&str] = &["mock-identity", "mock-prefix-target", "mock-uppercase"];
const DEEPSEEK_MODELS: &[&str] = &[
    "deepseek-v4-flash",
    "deepseek-v4-pro",
    "deepseek-chat",
    "deepseek-reasoner",
];
const OPENROUTER_MODELS: &[&str] = &[
    "openrouter/auto",
    "deepseek/deepseek-v4-flash",
    "google/gemini-2.5-flash-lite",
    "google/gemini-2.5-flash",
];
const OPENAI_COMPATIBLE_MODELS: &[&str] = &["gpt-4o-mini", "gpt-4o"];
const CSRF_HEADER: &str = "x-bookforge-csrf";
const CSRF_TOKEN_PLACEHOLDER: &str = "__BOOKFORGE_CSRF_TOKEN__";
const DASHBOARD_CONTENT_SECURITY_POLICY: &str = "default-src 'none'; base-uri 'none'; connect-src 'self'; font-src 'self'; form-action 'none'; frame-ancestors 'none'; img-src 'self' data:; media-src 'self'; object-src 'none'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'";

/// Monotonic suffix for estimate temp files, so two uploads landing in the same
/// millisecond never collide on a path (and delete each other's input mid-parse).
static ESTIMATE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[derive(Debug, clap::Args)]
pub struct ServeArgs {
    /// Address to bind. Must be loopback because the dashboard is unauthenticated
    /// and can accept provider API keys for child translation runs.
    #[arg(long, default_value = "127.0.0.1:8765")]
    pub bind: String,

    /// Open the dashboard in your default browser once the server is up.
    #[arg(long)]
    pub open: bool,

    /// Server-sent-events refresh interval in milliseconds.
    #[arg(long, default_value_t = 250)]
    pub refresh_ms: u64,
}

#[derive(Clone)]
struct AppState {
    refresh: Duration,
    csrf_token: String,
    host_port: u16,
    /// Root for browser uploads and audiobook operation directories. Production
    /// uses [`UPLOAD_DIR`]; tests inject a temp directory so route coverage does
    /// not race through the process-global working directory.
    upload_dir: PathBuf,
    /// Provider → API key, supplied via the dashboard. Held only in memory for
    /// the lifetime of the server: never written to disk, never logged, and
    /// only injected into spawned runs through the child's environment.
    keys: Arc<Mutex<HashMap<String, String>>>,
    /// Recently fetched public voice metadata. API keys remain exclusively in
    /// `keys` (or the provider environment) and are never cached here.
    elevenlabs_voices: Arc<Mutex<Option<ElevenLabsVoiceCache>>>,
    /// Browser-launched audiobook operations that can still be cancelled.
    audio_cancels: Arc<Mutex<HashMap<String, tokio_util::sync::CancellationToken>>>,
    /// Path to the job store's sqlite database, resolved once when the server
    /// (or, in tests, a router) is constructed. Handlers open a fresh
    /// [`JobStore`] against this path per request rather than calling
    /// [`JobStore::open_default`] directly, so the resolved path doesn't
    /// depend on the process-global current directory at request time — this
    /// keeps production behavior identical (same default relative path,
    /// resolved once at startup instead of per-request) while letting tests
    /// point a router at an isolated temp-dir store without touching CWD.
    store_path: PathBuf,
    /// Age budget used by dashboard lease reads. Production installs the
    /// three-second worker liveness budget; router tests can inject another.
    runtime_lease_stale_after: Duration,
    /// Per-job locks serializing manual corrections. Applying one correction is
    /// a read-modify-write over the whole book: it loads every block
    /// translation, merges one segment, stages a rebuilt EPUB, saves the
    /// segment, then renames the staged file over the output. Two browser tabs
    /// correcting different segments can otherwise both read the same
    /// pre-correction snapshot, and whichever renames last publishes an EPUB
    /// missing the other's edit — while SQLite retains both, so the store and
    /// the book silently disagree.
    correction_locks: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    #[cfg(test)]
    resume_launches: Option<Arc<std::sync::atomic::AtomicUsize>>,
    #[cfg(test)]
    resume_child_environments: Option<Arc<Mutex<Vec<CapturedChildEnvironment>>>>,
    #[cfg(test)]
    audio_restart_cancels: Option<Arc<Mutex<Vec<u32>>>>,
}

#[cfg(test)]
type CapturedChildEnvironment = HashMap<OsString, Option<OsString>>;

#[derive(Clone)]
struct ElevenLabsVoiceCache {
    fetched_at: Instant,
    voices: Vec<bookforge_audio::ElevenLabsVoice>,
}

/// The default job store path, relative to the current directory — identical
/// to what [`JobStore::open_default`] uses internally.
fn default_store_path() -> PathBuf {
    PathBuf::from(".bookforge/jobs.sqlite")
}

/// Ensure the process working directory can hold the `.bookforge` data root.
///
/// If the current directory already accepts a `.bookforge` write we keep it
/// (preserving the CLI convention of storing runs alongside the project). If it
/// doesn't — the typical outcome when the app is launched from the desktop
/// shell with a read-only working directory — we move to a per-user data
/// directory and switch the process there, so every CWD-relative `.bookforge`
/// path (uploads, the job store, run outputs, and any child the dashboard
/// spawns, which inherits this directory) resolves to a writable location.
fn ensure_writable_workdir() -> Result<()> {
    if data_root_is_writable(std::path::Path::new(".")) {
        return Ok(());
    }

    let fallback =
        stable_data_dir().context("could not determine a writable data directory for BookForge")?;
    std::fs::create_dir_all(&fallback)
        .with_context(|| format!("failed to create data directory {}", fallback.display()))?;
    if !data_root_is_writable(&fallback) {
        return Err(anyhow::anyhow!(
            "data directory {} is not writable",
            fallback.display()
        ));
    }
    std::env::set_current_dir(&fallback)
        .with_context(|| format!("failed to switch to data directory {}", fallback.display()))?;
    println!(
        "  working directory was not writable; storing data in {}",
        fallback.display()
    );
    Ok(())
}

/// Return true when a `.bookforge` directory can be created and written under
/// `base`. A directory that merely exists isn't enough — it may be read-only —
/// so this probes with an actual file write and cleans up after itself.
fn data_root_is_writable(base: &std::path::Path) -> bool {
    let root = base.join(".bookforge");
    if std::fs::create_dir_all(&root).is_err() {
        return false;
    }
    let probe = root.join(".write-probe");
    match std::fs::write(&probe, b"ok") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// A stable, per-user directory to fall back to when the working directory is
/// read-only. Prefers the platform's local application-data location and falls
/// back to the user's home directory.
fn stable_data_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(local) = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()) {
            return Some(PathBuf::from(local).join("BookForge"));
        }
        if let Some(profile) = std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()) {
            return Some(PathBuf::from(profile).join("BookForge"));
        }
        None
    }
    #[cfg(not(windows))]
    {
        if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
            return Some(PathBuf::from(xdg).join("bookforge"));
        }
        if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
            return Some(PathBuf::from(home).join(".local/share/bookforge"));
        }
        None
    }
}

pub async fn run(args: ServeArgs) -> Result<()> {
    // The dashboard and every run it spawns write their state under a
    // `.bookforge` directory resolved relative to the process working
    // directory. When BookForge is launched from the desktop shell — a Start
    // Menu shortcut, an installer, or a URL/protocol handler — that working
    // directory is often a location the user cannot write to (e.g.
    // `C:\Windows\System32` or `C:\Program Files\…`), which surfaced as
    // "Access is denied. (os error 5)" the moment a translation tried to
    // persist its upload. Relocate to a stable, per-user data directory when
    // the current one isn't writable so the web flow works regardless of how
    // the process was started. A writable CWD (the usual CLI case) is left
    // untouched, so `bookforge serve` from a project folder still keeps its
    // `.bookforge` there.
    ensure_writable_workdir()?;

    let addr: SocketAddr = args
        .bind
        .parse()
        .with_context(|| format!("invalid --bind address '{}'", args.bind))?;
    require_loopback_bind(addr)?;

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;
    let local = listener.local_addr().unwrap_or(addr);
    let state = AppState {
        refresh: Duration::from_millis(args.refresh_ms.clamp(50, 5_000)),
        csrf_token: generate_csrf_token()?,
        host_port: local.port(),
        upload_dir: PathBuf::from(UPLOAD_DIR),
        keys: Arc::new(Mutex::new(HashMap::new())),
        elevenlabs_voices: Arc::new(Mutex::new(None)),
        audio_cancels: Arc::new(Mutex::new(HashMap::new())),
        store_path: default_store_path(),
        runtime_lease_stale_after: crate::control::RUNTIME_LEASE_STALE_AFTER,
        correction_locks: Arc::new(Mutex::new(HashMap::new())),
        #[cfg(test)]
        resume_launches: None,
        #[cfg(test)]
        resume_child_environments: None,
        #[cfg(test)]
        audio_restart_cancels: None,
    };

    let app = dashboard_router(state);
    let url = format!("http://{local}/");

    println!("BookForge dashboard listening on {url}");
    println!("  press Ctrl-C to stop");

    if args.open
        && let Err(err) = open_in_browser(&url)
    {
        eprintln!("could not open browser automatically: {err}");
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("dashboard server error")?;
    Ok(())
}

fn dashboard_router(state: AppState) -> Router {
    let host_state = state.clone();
    Router::new()
        .merge(assets::routes())
        .merge(jobs::routes())
        .merge(options::routes())
        .merge(audio::routes())
        .merge(translation::routes())
        .merge(glossary::routes())
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
        .layer(middleware::from_fn_with_state(
            host_state,
            validate_dashboard_host,
        ))
        .with_state(state)
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn child_exit_status_after(
    child: &mut tokio::process::Child,
    delay: Duration,
) -> Result<Option<ExitStatus>> {
    tokio::time::sleep(delay).await;
    child
        .try_wait()
        .context("failed to check translation process status")
}

fn lock_keys(state: &AppState) -> Result<MutexGuard<'_, HashMap<String, String>>> {
    state
        .keys
        .lock()
        .map_err(|_| anyhow::anyhow!("dashboard API key store is unavailable"))
}

fn field_value(fields: &HashMap<String, String>, key: &str) -> Option<String> {
    fields
        .get(key)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn strip_epub_suffix(name: &str) -> &str {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    base.strip_suffix(".epub")
        .or_else(|| base.strip_suffix(".EPUB"))
        .unwrap_or(base)
}

/// Reduce arbitrary user text to a safe single path component.
fn sanitize_component(input: &str) -> String {
    let cleaned: String = input
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-');
    if trimmed.is_empty() {
        "book".to_string()
    } else {
        trimmed.chars().take(60).collect()
    }
}

fn bad_request(message: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": message }))).into_response()
}

fn forbidden(message: &str) -> Response {
    (StatusCode::FORBIDDEN, Json(json!({ "error": message }))).into_response()
}

/// Wraps any error so handlers can use `?`; renders as a 500 JSON body.
struct AppError(anyhow::Error);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": self.0.to_string() })),
        )
            .into_response()
    }
}

impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(err: E) -> Self {
        Self(err.into())
    }
}

// ---------------------------------------------------------------------------
// Browser launch (mirrors review.rs)
// ---------------------------------------------------------------------------

fn open_in_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let status = Command::new("open").arg(url).status()?;

    #[cfg(target_os = "windows")]
    let status = Command::new("cmd")
        .args(["/C", "start", "", url])
        .status()?;

    #[cfg(all(unix, not(target_os = "macos")))]
    let status = Command::new("xdg-open").arg(url).status()?;

    if !status.success() {
        anyhow::bail!("browser launcher exited with failure");
    }
    Ok(())
}

#[cfg(test)]
mod tests;
