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
    extract::{DefaultBodyLimit, Multipart, Path as AxumPath, Query, RawQuery, Request, State},
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
mod entities;
mod glossary;
mod jobs;
mod options;
mod security;
mod styles;
mod translation;

use super::{audiobook, correct, estimate, reconfigure, review, validate};
use security::{
    AuthState, BootstrapOutcome, apply_security_headers, auth_logout, build_session_cookie,
    is_cross_site_browser_request, reject_mutation, require_loopback_bind, session_cookie_from,
    unauthorized, validate_dashboard_host,
};
use translation::{
    configure_dashboard_child_environment, provider_key_env, resolve_dashboard_provider_key,
};

/// Where browser-launched uploads and their outputs are written.
const UPLOAD_DIR: &str = ".bookforge/serve-uploads";

/// Cap on a multipart upload body (EPUBs in the regression corpus reach ~11 MB).
const MAX_UPLOAD_BYTES: usize = 64 * 1024 * 1024;

/// Cap on dashboard operations that spawn work sharing remembered provider
/// keys (translation launches, resumes, audiobook launches). A stray tab or a
/// looping script must not be able to start unbounded billable runs in one
/// burst; the slot is held only for the launch handshake itself.
const MAX_CONCURRENT_DASHBOARD_LAUNCHES: usize = 4;

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
const DASHBOARD_CONTENT_SECURITY_POLICY: &str = "default-src 'none'; base-uri 'none'; connect-src 'self'; font-src 'self'; form-action 'none'; frame-ancestors 'none'; img-src 'self' data:; media-src 'self'; object-src 'none'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'";

/// Monotonic suffix for launch tags and estimate temp files, so two uploads
/// landing in the same millisecond never collide on a path (and delete each
/// other's input mid-parse).
static LAUNCH_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn next_launch_seq() -> u64 {
    LAUNCH_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

#[derive(Debug, clap::Args)]
pub struct ServeArgs {
    /// Address to bind. Must be loopback because the dashboard serves private
    /// book text and can accept provider API keys for child runs.
    #[arg(long, default_value = "127.0.0.1:8765")]
    pub bind: String,

    /// Open the dashboard in your default browser once the server is up.
    #[arg(long)]
    pub open: bool,

    /// Disable the session-token login printed at startup.
    ///
    /// Escape hatch for environments where the console is not reachable (for
    /// example a container orchestrator that only forwards the port). With the
    /// token disabled, any local process can spend remembered provider keys,
    /// so prefer an SSH tunnel plus the default token flow instead.
    #[arg(long)]
    pub no_auth: bool,

    /// Server-sent-events refresh interval in milliseconds.
    #[arg(long, default_value_t = 250)]
    pub refresh_ms: u64,
}

#[derive(Clone)]
struct AppState {
    refresh: Duration,
    /// Server-side authentication: bootstrap-token exchange, in-memory session
    /// store, and the `--no-auth` escape hatch. Default-on per H-5.
    auth: Arc<AuthState>,
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
    /// Held slots for dashboard launch operations (see
    /// [`MAX_CONCURRENT_DASHBOARD_LAUNCHES`]).
    launch_slots: Arc<Mutex<usize>>,
    #[cfg(test)]
    resume_launches: Option<Arc<std::sync::atomic::AtomicUsize>>,
    #[cfg(test)]
    resume_child_environments: Option<Arc<Mutex<Vec<CapturedChildEnvironment>>>>,
    /// Test hook mirroring [`Self::resume_launches`] for audiobook
    /// retry-failed relaunches: when set, endpoint tests record the launch
    /// count instead of exec'ing this binary as a child.
    #[cfg(test)]
    retry_launches: Option<Arc<std::sync::atomic::AtomicUsize>>,
    /// Test hook: when set and true, the retry-failed handoff fails at its
    /// spawn seam so tests can prove the atomic claim and the launch slot are
    /// released on that error path.
    #[cfg(test)]
    retry_fail_spawns: Option<Arc<std::sync::atomic::AtomicBool>>,
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
    ensure_private_dir_under(&fallback, &fallback)
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
///
/// The probe creates the root through [`ensure_private_dir_under`] so the
/// directory is 0700 on Unix from the first second of its existence (H-6):
/// an earlier version used plain `create_dir_all`, which shipped a
/// world-readable `.bookforge` until (or unless) something else tightened it.
fn data_root_is_writable(base: &std::path::Path) -> bool {
    let root = base.join(".bookforge");
    if ensure_private_dir_under(base, &root).is_err() {
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

/// Tighten the serve-owned `.bookforge` root under the current working
/// directory to 0700 (Unix), including one that a previous, world-readable
/// run created. Every other component that lands inside the root goes through
/// these helpers too — uploads, launch directories, private files.
fn ensure_private_data_root() -> Result<()> {
    ensure_private_dir_under(Path::new("."), Path::new(".bookforge"))
        .context("failed to prepare the .bookforge data directory")
}

/// Drain store-open diagnostics once per serve process so schema-tolerance
/// notes (unknown legacy statuses, skipped hardening) reach the operator
/// instead of dying in the store's queue, matching the CLI surface behavior.
fn drain_store_diagnostics_once() {
    match bookforge_store::JobStore::open_default() {
        Ok(store) => {
            for diagnostic in store.take_diagnostics() {
                tracing::warn!(surface = "serve", "{diagnostic}");
            }
        }
        Err(error) => {
            tracing::warn!(
                surface = "serve",
                "store open failed during diagnostics drain: {error}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Private-directory and private-file helpers (H-6)
//
// Mirrors translate/snapshot.rs (`create_private_dir_all`, the 0600 snapshot
// writer): recursive creation is asked for mode 0700 on Unix, and any
// directory that already exists with looser bits (a leftover from an older,
// pre-hardening release) is tightened in place. Non-Unix targets have no
// equivalent permission model, so the chmod halves degrade to no-ops.
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn create_private_dir_all(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
}

#[cfg(not(unix))]
fn create_private_dir_all(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)
}

/// Ensure `path` exists as a private directory whose every component between
/// it and `base` (inclusive on both ends) carries private permissions —
/// creation happens with 0700 and pre-existing loose components are tightened.
fn ensure_private_dir_under(base: &Path, path: &Path) -> std::io::Result<()> {
    create_private_dir_all(path)?;
    tighten_private_under(base, path);
    Ok(())
}

#[cfg(unix)]
fn tighten_private_under(base: &Path, path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let mut current = Some(path);
    while let Some(dir) = current {
        // Best effort: tightening must never turn a writable-data-dir probe
        // or an upload into a hard failure.
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        if dir == base {
            break;
        }
        current = dir.parent();
    }
}

#[cfg(not(unix))]
fn tighten_private_under(_base: &Path, _path: &Path) {}

/// A self-deleting scratch directory for untrusted upload content (SERVE-5).
///
/// Production code cannot use the `tempfile` crate (it is only a
/// dev-dependency of this crate and Cargo.toml changes are outside this
/// workstream), so this is a ~20-line equivalent holding the properties that
/// matter: unpredictable per-request names (PID + monotonic counter + random
/// bytes), creation with owner-only permissions on Unix, and deletion on drop
/// whether the parse succeeds, fails, or panics.
struct PrivateTempDir {
    path: PathBuf,
}

impl Drop for PrivateTempDir {
    fn drop(&mut self) {
        // Best effort: leftover directories are swept by the OS tmp cleaner.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

impl PrivateTempDir {
    fn create() -> Result<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};

        static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);
        for _ in 0..8 {
            let mut randomness = [0u8; 8];
            getrandom::fill(&mut randomness)
                .context("failed to generate temporary directory name")?;
            let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "bookforge-private-{}-{seq}-{}.tmp",
                std::process::id(),
                u64::from_be_bytes(randomness),
            ));

            #[cfg(unix)]
            let created = {
                use std::os::unix::fs::DirBuilderExt;
                std::fs::DirBuilder::new().mode(0o700).create(&path)
            };
            #[cfg(not(unix))]
            let created = std::fs::create_dir(&path);

            match created {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(anyhow::anyhow!(
            "could not allocate a private temporary directory"
        ))
    }
}

/// Write `bytes` to `path` with owner-only permissions (0600 on Unix),
/// including over a pre-existing, previously-loose file. Used for every EPUB
/// the dashboard persists to disk.
fn write_private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(bytes)?;
        // The creation mode already lands at 0600; normalize explicitly in
        // case the overwrite reused a stale 0644 file from an older release.
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes)
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
    ensure_private_data_root()?;
    drain_store_diagnostics_once();

    let addr: SocketAddr = args
        .bind
        .parse()
        .with_context(|| format!("invalid --bind address '{}'", args.bind))?;
    require_loopback_bind(addr)?;

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;
    let local = listener.local_addr().unwrap_or(addr);
    let auth_enabled = !args.no_auth;
    let auth = AuthState::new(auth_enabled)?;
    let bootstrap_url_token = auth.bootstrap_token();
    let state = AppState {
        // Same floor as `watch` (crate::commands::MIN_REFRESH_MS): one flag
        // value, one floor, whichever UI consumes it.
        refresh: Duration::from_millis(
            args.refresh_ms
                .clamp(crate::commands::MIN_REFRESH_MS, 5_000),
        ),
        auth: Arc::new(auth),
        host_port: local.port(),
        upload_dir: PathBuf::from(UPLOAD_DIR),
        keys: Arc::new(Mutex::new(HashMap::new())),
        elevenlabs_voices: Arc::new(Mutex::new(None)),
        audio_cancels: Arc::new(Mutex::new(HashMap::new())),
        store_path: default_store_path(),
        runtime_lease_stale_after: crate::control::RUNTIME_LEASE_STALE_AFTER,
        correction_locks: Arc::new(Mutex::new(HashMap::new())),
        launch_slots: Arc::new(Mutex::new(0)),
        #[cfg(test)]
        resume_launches: None,
        #[cfg(test)]
        resume_child_environments: None,
        #[cfg(test)]
        retry_launches: None,
        #[cfg(test)]
        retry_fail_spawns: None,
        #[cfg(test)]
        audio_restart_cancels: None,
    };

    let app = dashboard_router(state);
    // With auth enabled, only this bootstrap URL — which embeds the one-time
    // bootstrap token as a query parameter — may enter the dashboard. The
    // exchange mints an HttpOnly session cookie and redirects to the clean
    // root. Never echo the bootstrap token anywhere else.
    let url = if auth_enabled {
        format!("http://{local}/?token={bootstrap_url_token}")
    } else {
        format!("http://{local}/")
    };

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
    let auth_state = state.clone();
    Router::new()
        .route("/api/auth/logout", post(auth_logout))
        .merge(assets::routes())
        .merge(jobs::routes())
        .merge(options::routes())
        .merge(audio::routes())
        .merge(translation::routes())
        .merge(glossary::routes())
        .merge(styles::routes())
        .merge(entities::routes())
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
        // H-5 / SERVE-1: every route outside the `/` bootstrap exchange and
        // the `/api/auth/logout` session-management endpoint requires a live
        // in-memory session cookie; hardened headers are stamped onto every
        // response (SERVE-9). The host allowlist stays outermost so a forged
        // Host is still rejected before authentication is even attempted.
        .layer(middleware::from_fn_with_state(
            auth_state,
            enforce_dashboard_access,
        ))
        .layer(middleware::from_fn_with_state(
            host_state,
            validate_dashboard_host,
        ))
        .with_state(state)
}

/// Gate middleware for the whole dashboard.
///
/// - Auth-on: everything except the `/` bootstrap exchange (which issues the
///   session cookie) and `/api/auth/logout` (which a stale browser must be
///   able to reach) requires the HttpOnly session cookie. Wrong/missing
///   cookies get a bare 401 with no detail.
/// - Security headers ride along on every response, not just the index page.
async fn enforce_dashboard_access(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let mut response = if state.auth.enabled && !is_unauthenticated_dashboard_route(&request) {
        if !state
            .auth
            .has_session(session_cookie_from(request.headers()))
        {
            let mut response = unauthorized();
            apply_security_headers(&mut response);
            return response;
        }
        next.run(request).await
    } else {
        next.run(request).await
    };
    apply_security_headers(&mut response);
    response
}

/// Routes reachable without an authenticated session: the `/` bootstrap
/// exchange and the logout endpoint (which must be callable to clear a stale
/// cookie even when the session is already gone).
fn is_unauthenticated_dashboard_route(request: &Request) -> bool {
    matches!(request.uri().path(), "/" | "/api/auth/logout")
}

// ---------------------------------------------------------------------------
// Shared request-safety helpers
// ---------------------------------------------------------------------------

/// Strict allowlist for job ids before they touch filesystem paths
/// (SERVE-4; mirrors [`crate::commands::serve::audio`]'s audiobook-id check).
/// Real ids look like `job_<unix-nanos>_<12 hex>`, so alphanumerics plus `-`
/// and `_` accept everything legitimate while rejecting traversal, slashes,
/// and percent-decoded junk outright.
pub(super) fn valid_job_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 160
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// Filesystem-safe sanity bound for segment ids. Unlike job ids these reach
/// SQL more than disk, so a looser but still path-hostile charset suffices:
/// no separators, no traversal segments, no NULs, bounded length.
fn valid_segment_id(segment_id: &str) -> bool {
    !segment_id.is_empty()
        && segment_id.len() <= 200
        && !segment_id.contains(['/', '\\'])
        && segment_id != ".."
}

fn invalid_job_id_response() -> Response {
    bad_request("invalid job id")
}

/// A counted slot holding one dashboard launch above the
/// [`MAX_CONCURRENT_DASHBOARD_LAUNCHES`] cap. Dropping the guard releases the
/// slot, so panics or early returns cannot strand capacity.
struct LaunchSlotGuard {
    slots: Arc<Mutex<usize>>,
}

impl Drop for LaunchSlotGuard {
    fn drop(&mut self) {
        if let Ok(mut held) = self.slots.lock() {
            *held = held.saturating_sub(1);
        }
    }
}

enum LaunchSlot {
    Acquired(LaunchSlotGuard),
    Exhausted,
}

fn try_acquire_launch_slot(state: &AppState) -> Result<LaunchSlot> {
    let mut held = state
        .launch_slots
        .lock()
        .map_err(|_| anyhow::anyhow!("dashboard launch registry is unavailable"))?;
    if *held >= MAX_CONCURRENT_DASHBOARD_LAUNCHES {
        return Ok(LaunchSlot::Exhausted);
    }
    *held += 1;
    Ok(LaunchSlot::Acquired(LaunchSlotGuard {
        slots: Arc::clone(&state.launch_slots),
    }))
}

fn launch_slot_exhausted() -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        Json(json!({
            "error": "another dashboard launch is already starting; try again in a moment",
        })),
    )
        .into_response()
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
///
/// The response carries a generic message plus a short correlation reference
/// instead of the anyhow chain (SERVE-10): chains have repeatedly leaked
/// absolute store/run paths that are useless — and mildly informative — to an
/// HTTP client. The full chain still reaches the server console for triage.
struct AppError(anyhow::Error);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let chain = format!("{:?}\n{:#}", self.0, self.0);
        let reference = error_reference(&chain);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "error": "internal server error",
                "reference": reference,
            })),
        )
            .into_response()
    }
}

impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(err: E) -> Self {
        let err: anyhow::Error = err.into();
        eprintln!("[serve] internal error: {err:#}");
        Self(err)
    }
}

/// A short stable digest of the error chain so a user-reported `reference`
/// can be matched against the console log without exposing the chain itself.
fn error_reference(detail: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in detail.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("{hash:016x}")
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
