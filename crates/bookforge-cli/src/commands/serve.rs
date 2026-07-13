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
    collections::HashMap,
    convert::Infallible,
    net::SocketAddr,
    path::PathBuf,
    process::{Command, ExitStatus},
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
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

/// Where browser-launched uploads and their outputs are written.
const UPLOAD_DIR: &str = ".bookforge/serve-uploads";

/// Cap on a multipart upload body (EPUBs in the regression corpus reach ~11 MB).
const MAX_UPLOAD_BYTES: usize = 64 * 1024 * 1024;

/// Briefly check a detached translation child before reporting launch success.
const CHILD_STARTUP_CHECK: Duration = Duration::from_millis(150);

/// Cloud providers the dashboard form offers, paired with the env var their
/// runs read a key from when one is configured in the operator's environment.
const PROVIDER_KEY_ENVS: &[(&str, &str)] = &[
    ("deepseek", "DEEPSEEK_API_KEY"),
    ("openrouter", "OPENROUTER_API_KEY"),
    ("openai-compatible", "OPENAI_API_KEY"),
];

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
    /// Provider → API key, supplied via the dashboard. Held only in memory for
    /// the lifetime of the server: never written to disk, never logged, and
    /// only injected into spawned runs through the child's environment.
    keys: Arc<Mutex<HashMap<String, String>>>,
    /// Path to the job store's sqlite database, resolved once when the server
    /// (or, in tests, a router) is constructed. Handlers open a fresh
    /// [`JobStore`] against this path per request rather than calling
    /// [`JobStore::open_default`] directly, so the resolved path doesn't
    /// depend on the process-global current directory at request time — this
    /// keeps production behavior identical (same default relative path,
    /// resolved once at startup instead of per-request) while letting tests
    /// point a router at an isolated temp-dir store without touching CWD.
    store_path: PathBuf,
    #[cfg(test)]
    resume_launches: Option<Arc<std::sync::atomic::AtomicUsize>>,
}

/// The default job store path, relative to the current directory — identical
/// to what [`JobStore::open_default`] uses internally.
fn default_store_path() -> PathBuf {
    PathBuf::from(".bookforge/jobs.sqlite")
}

pub async fn run(args: ServeArgs) -> Result<()> {
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
        keys: Arc::new(Mutex::new(HashMap::new())),
        store_path: default_store_path(),
        #[cfg(test)]
        resume_launches: None,
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
        .route("/", get(index))
        .route("/api/jobs", get(list_jobs))
        .route("/api/jobs/{id}", get(job_detail))
        .route(
            "/api/jobs/{id}/reconfigure",
            get(job_reconfigure).post(update_job_reconfigure),
        )
        .route("/api/jobs/{id}/events", get(job_events))
        .route("/api/jobs/{id}/review", get(job_review))
        .route(
            "/api/jobs/{id}/segments/{segment_id}/translation",
            post(save_manual_translation),
        )
        .route(
            "/api/jobs/{id}/segments/{segment_id}/flag",
            post(set_segment_flag),
        )
        .route(
            "/api/jobs/{id}/segments/{segment_id}/retry",
            post(retry_segment_with_guidance),
        )
        .route("/api/jobs/{id}/validate", post(job_validate))
        .route("/api/jobs/{id}/retry", post(retry_job))
        .route("/api/jobs/{id}/pause", post(pause_job))
        .route("/api/jobs/{id}/resume", post(resume_job))
        .route("/api/jobs/{id}/stop", post(stop_job))
        .route("/api/options", get(dashboard_options))
        .route("/api/providers", get(provider_status))
        .route("/api/estimate", post(estimate_translate))
        .route("/api/translate", post(launch_translate))
        .route("/api/glossary", get(list_glossary).post(add_glossary))
        .route("/api/glossary/{id}", delete(remove_glossary))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
        .layer(middleware::from_fn_with_state(
            host_state,
            validate_dashboard_host,
        ))
        .with_state(state)
}

async fn validate_dashboard_host(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    if dashboard_host_allowed(request.headers(), state.host_port) {
        return next.run(request).await;
    }
    forbidden("dashboard host header rejected")
}

fn dashboard_host_allowed(headers: &HeaderMap, port: u16) -> bool {
    headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|host| dashboard_host_value_allowed(host, port))
}

fn dashboard_host_value_allowed(host: &str, port: u16) -> bool {
    if host.is_empty()
        || host
            .bytes()
            .any(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
    {
        return false;
    }

    if let Some(rest) = host.strip_prefix('[') {
        let Some((addr, suffix)) = rest.split_once(']') else {
            return false;
        };
        return addr == "::1" && suffix == format!(":{port}");
    }

    let Some((name, host_port)) = host.rsplit_once(':') else {
        return false;
    };
    if name.contains(':') || host_port != port.to_string() {
        return false;
    }

    name == "127.0.0.1" || name.eq_ignore_ascii_case("localhost")
}

fn require_loopback_bind(addr: SocketAddr) -> Result<()> {
    if addr.ip().is_loopback() {
        return Ok(());
    }
    anyhow::bail!(
        "--bind must use a loopback address such as 127.0.0.1:8765; use an SSH tunnel for remote access"
    );
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn index(State(state): State<AppState>) -> Html<String> {
    Html(DASHBOARD_HTML.replace(CSRF_TOKEN_PLACEHOLDER, &state.csrf_token))
}

async fn list_jobs(State(state): State<AppState>) -> Result<Json<Vec<JobListItem>>, AppError> {
    let store_path = state.store_path.clone();
    let items = tokio::task::spawn_blocking(move || -> Result<Vec<JobListItem>> {
        let store = JobStore::open(store_path)?;
        Ok(store
            .list_job_summaries()?
            .into_iter()
            .map(|(job, summary)| JobListItem::new(&job, &summary))
            .collect())
    })
    .await??;
    Ok(Json(items))
}

async fn job_detail(
    AxumPath(id): AxumPath<String>,
    State(state): State<AppState>,
) -> Result<Response, AppError> {
    let lookup = id.clone();
    let store_path = state.store_path.clone();
    let detail = tokio::task::spawn_blocking(move || -> Result<Option<JobDetail>> {
        let store = JobStore::open(store_path)?;
        let job = store.get_job(&lookup)?;
        let events_path = events_path_for(job.as_ref(), &lookup);
        if job.is_none() && !events_path.exists() {
            return Ok(None);
        }
        let mut tailer = EventLogTailer::new(events_path);
        let mut state = RunState::default();
        for event in tailer.poll()? {
            state.fold(&event);
        }
        Ok(Some(JobDetail::new(lookup, job, state)))
    })
    .await??;

    match detail {
        Some(detail) => Ok(Json(detail).into_response()),
        None => Ok((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("no job '{id}'") })),
        )
            .into_response()),
    }
}

async fn job_reconfigure(
    AxumPath(id): AxumPath<String>,
    State(state): State<AppState>,
) -> Result<Response, AppError> {
    let store_path = state.store_path.clone();
    let outcome =
        tokio::task::spawn_blocking(move || runtime_settings_view(&store_path, &id)).await?;
    match outcome {
        Ok(Some(view)) => Ok(Json(view).into_response()),
        Ok(None) => Ok((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no such job or run snapshot" })),
        )
            .into_response()),
        Err(error) => Ok(bad_request(&error.to_string())),
    }
}

async fn update_job_reconfigure(
    AxumPath(id): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(incoming): Json<super::reconfigure::RunConfigOverrides>,
) -> Result<Response, AppError> {
    if let Some(response) = reject_mutation(&headers, &state) {
        return Ok(response);
    }
    if incoming.is_empty() {
        return Ok(bad_request("select at least one runtime setting"));
    }
    let store_path = state.store_path.clone();
    let outcome = tokio::task::spawn_blocking(move || -> Result<RuntimeSettingsView> {
        let store = JobStore::open(&store_path)?;
        let Some(job) = store.get_job(&id)? else {
            anyhow::bail!("no such job");
        };
        if !matches!(job.status.as_str(), "running" | "paused" | "stopped") {
            anyhow::bail!(
                "job '{}' is {}; runtime settings are editable only while running, paused, or stopped",
                id,
                job.status
            );
        }
        if store.load_job_config_snapshot(&id)?.is_none() {
            anyhow::bail!("job '{}' has no resumable run snapshot", id);
        }
        let (_path, written) =
            super::reconfigure::write_merged_overrides_for_job(&id, incoming)?;
        let mut view = runtime_settings_view(&store_path, &id)?
            .ok_or_else(|| anyhow::anyhow!("job disappeared after reconfiguration"))?;
        view.revision = written.revision;
        Ok(view)
    })
    .await?;

    match outcome {
        Ok(view) => Ok(Json(view).into_response()),
        Err(error) => Ok(bad_request(&error.to_string())),
    }
}

async fn job_events(
    AxumPath(id): AxumPath<String>,
    State(state): State<AppState>,
) -> Sse<impl futures_core::Stream<Item = Result<Event, Infallible>>> {
    let refresh = state.refresh;
    let path = resolve_events_path(id, state.store_path.clone()).await;

    let stream = async_stream::stream! {
        let mut tailer = EventLogTailer::new(path);
        let mut run = RunState::default();
        let mut ticker = tokio::time::interval(refresh);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last = String::new();

        loop {
            ticker.tick().await;
            if let Ok(events) = tailer.poll() {
                for event in events {
                    run.fold(&event);
                }
            }
            if let Ok(payload) = serde_json::to_string(&run)
                && payload != last
            {
                last = payload.clone();
                yield Ok(Event::default().event("state").data(payload));
            }
            if run.finished {
                yield Ok(Event::default().event("done").data("done"));
                break;
            }
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// Serve a job's side-by-side review as JSON. Shares
/// [`generate_review_document`](super::review::generate_review_document) with the
/// CLI `review` command; the browser renders it into the Review screen. Errors
/// (unknown job, or a job that predates run-config snapshots) become 404s.
async fn job_review(
    AxumPath(id): AxumPath<String>,
    State(state): State<AppState>,
) -> Result<Response, AppError> {
    let store_path = state.store_path.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let store = JobStore::open(store_path)?;
        super::review::generate_review_document(&store, &id)
    })
    .await?;

    match outcome {
        Ok(document) => Ok(Json(document).into_response()),
        Err(err) => Ok((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": err.to_string() })),
        )
            .into_response()),
    }
}

#[derive(Debug, Deserialize)]
struct ManualCorrectionRequest {
    blocks: Vec<super::correct::CorrectionBlock>,
}

async fn save_manual_translation(
    AxumPath((id, segment_id)): AxumPath<(String, String)>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ManualCorrectionRequest>,
) -> Result<Response, AppError> {
    if let Some(response) = reject_mutation(&headers, &state) {
        return Ok(response);
    }
    if request.blocks.is_empty() {
        return Ok(bad_request("at least one corrected block is required"));
    }

    let store_path = state.store_path.clone();
    let outcome = tokio::task::spawn_blocking(move || -> Result<_> {
        let store = JobStore::open(store_path)?;
        super::correct::correct_job_segment(
            &store,
            &id,
            &segment_id,
            super::correct::CorrectionPayload::Blocks(request.blocks),
        )
    })
    .await?;

    match outcome {
        Ok(outcome) => Ok(Json(outcome).into_response()),
        Err(err) => Ok(bad_request(&err.to_string())),
    }
}

#[derive(Debug, Deserialize)]
struct SegmentFlagRequest {
    flagged: bool,
}

async fn set_segment_flag(
    AxumPath((id, segment_id)): AxumPath<(String, String)>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<SegmentFlagRequest>,
) -> Result<Response, AppError> {
    if let Some(response) = reject_mutation(&headers, &state) {
        return Ok(response);
    }
    let store_path = state.store_path.clone();
    let outcome = tokio::task::spawn_blocking(move || -> Result<()> {
        let store = JobStore::open(store_path)?;
        store.set_dashboard_segment_flag(&id, &segment_id, request.flagged)?;
        Ok(())
    })
    .await?;
    match outcome {
        Ok(()) => Ok(Json(json!({ "flagged": request.flagged })).into_response()),
        Err(err) => Ok(bad_request(&err.to_string())),
    }
}

#[derive(Debug, Deserialize)]
struct SegmentRetryRequest {
    #[serde(default)]
    guidance: Option<String>,
}

async fn retry_segment_with_guidance(
    AxumPath((id, segment_id)): AxumPath<(String, String)>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<SegmentRetryRequest>,
) -> Result<Response, AppError> {
    if let Some(response) = reject_mutation(&headers, &state) {
        return Ok(response);
    }
    let store_path = state.store_path.clone();
    let outcome = tokio::task::spawn_blocking(move || -> Result<()> {
        let store = JobStore::open(store_path)?;
        store.request_segment_retry(&id, &segment_id, request.guidance.as_deref())?;
        Ok(())
    })
    .await?;
    match outcome {
        Ok(()) => Ok(Json(json!({ "retry_pending": true })).into_response()),
        Err(err) => Ok(bad_request(&err.to_string())),
    }
}

/// Validate a job's translated EPUB (BookForge structural validators + EPUBCheck)
/// and return the report. Shares [`validate_path`](super::validate::validate_path)
/// with the CLI `validate` command. EPUBCheck may be `unavailable` (no external
/// tool) — that's reported, not an error. POST + CSRF because it runs an external
/// process and is triggered explicitly ("Re-run check").
async fn job_validate(
    AxumPath(id): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    if let Some(response) = reject_mutation(&headers, &state) {
        return Ok(response);
    }

    let store_path = state.store_path.clone();
    let outcome = tokio::task::spawn_blocking(
        move || -> Result<Option<super::validate::ValidationReport>> {
            let store = JobStore::open(store_path)?;
            let Some(job) = store.get_job(&id)? else {
                return Ok(None);
            };
            if !job.output_path.exists() {
                anyhow::bail!(
                    "translated EPUB not found at {} — finish the run first",
                    job.output_path.display()
                );
            }
            Ok(Some(
                super::validate::validate_path(&job.output_path, false).report,
            ))
        },
    )
    .await?;

    match outcome {
        Ok(Some(report)) => Ok(Json(report).into_response()),
        Ok(None) => Ok((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no such job" })),
        )
            .into_response()),
        Err(err) => Ok(bad_request(&err.to_string())),
    }
}

async fn retry_job(
    AxumPath(id): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    if let Some(response) = reject_mutation(&headers, &state) {
        return Ok(response);
    }
    let store_path = state.store_path.clone();
    let retried = tokio::task::spawn_blocking(move || -> Result<usize> {
        let store = JobStore::open(store_path)?;
        Ok(store.retry_segments(&id, RetryScope::All)?)
    })
    .await??;
    Ok(Json(json!({ "retried": retried })).into_response())
}

async fn pause_job(
    AxumPath(id): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    control_job(id, state, headers, ControlCommand::Pause).await
}

async fn resume_job(
    AxumPath(id): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    if let Some(response) = reject_mutation(&headers, &state) {
        return Ok(response);
    }
    let store_path = state.store_path.clone();
    let lookup = id.clone();
    let action = tokio::task::spawn_blocking(move || -> Result<Option<(bool, bool, bool)>> {
        let store = JobStore::open(&store_path)?;
        let Some(job) = store.get_job(&lookup)? else {
            return Ok(None);
        };
        let live = matches!(
            crate::control::runtime_lease_state(&lookup),
            crate::control::RuntimeLeaseState::Fresh(_)
        );
        let resumable = !store.resumable_segment_ids(&lookup)?.is_empty()
            || (job_status_has_unfinished_pipeline_work(&job.status)
                && store.load_job_config_snapshot(&lookup)?.is_some());
        let force = !live && job.status == "paused";
        Ok(Some((live, resumable, force)))
    })
    .await??;
    let Some((live, resumable, force)) = action else {
        return Ok((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no such job" })),
        )
            .into_response());
    };
    if live {
        let path = crate::control::request_job_control(&id, ControlCommand::Resume)?;
        return Ok(Json(json!({
            "command": "resume",
            "mode": "signaled",
            "control_path": path,
        }))
        .into_response());
    }
    if !resumable {
        return Ok(bad_request(
            "the worker is not alive and this job has no resumable work",
        ));
    }

    let Some(mut launch_claim) = crate::control::RuntimeLaunchClaim::acquire(&id)? else {
        return Ok(Json(json!({
            "command": "resume",
            "mode": "launching",
        }))
        .into_response());
    };
    #[cfg(test)]
    if let Some(launches) = &state.resume_launches {
        launches.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        launch_claim.persist_until_worker();
        return Ok(Json(json!({
            "command": "resume",
            "mode": "spawned",
            "pid": 0,
            "forced": force,
        }))
        .into_response());
    }
    let executable =
        std::env::current_exe().context("failed to locate the BookForge executable")?;
    let mut command = tokio::process::Command::new(executable);
    command
        .arg("resume")
        .arg(&id)
        .arg("--ui")
        .arg("quiet")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if force {
        // A paused job normally expects to signal its original process. A
        // missing/stale lease proves that process is unavailable, so the
        // replacement must use the CLI's explicit dead-worker escape hatch.
        command.arg("--force");
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.as_std_mut().creation_flags(0x0800_0000);
    }
    let mut child = command.spawn().context("failed to launch resume worker")?;
    let pid = child.id();
    if let Some(status) = child_exit_status_after(&mut child, CHILD_STARTUP_CHECK).await?
        && !status.success()
    {
        return Ok(bad_request(&format!(
            "resume worker exited immediately with {status}"
        )));
    }
    launch_claim.persist_until_worker();
    Ok(Json(json!({
        "command": "resume",
        "mode": "spawned",
        "pid": pid,
        "forced": force,
    }))
    .into_response())
}

fn job_status_has_unfinished_pipeline_work(status: &str) -> bool {
    matches!(status, "running" | "paused" | "stopped")
}

async fn stop_job(
    AxumPath(id): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    control_job(id, state, headers, ControlCommand::Stop).await
}

async fn control_job(
    id: String,
    state: AppState,
    headers: HeaderMap,
    command: ControlCommand,
) -> Result<Response, AppError> {
    if let Some(response) = reject_mutation(&headers, &state) {
        return Ok(response);
    }
    let store_path = state.store_path.clone();
    let outcome = tokio::task::spawn_blocking(move || -> Result<Option<String>> {
        let store = JobStore::open(store_path)?;
        if store.get_job(&id)?.is_none() {
            return Ok(None);
        }
        if !matches!(
            crate::control::runtime_lease_state(&id),
            crate::control::RuntimeLeaseState::Fresh(_)
        ) {
            anyhow::bail!(
                "no live worker is available for {}; refresh the job and use Resume to launch one",
                command.as_str()
            );
        }
        let path = crate::control::request_job_control(&id, command)?;
        Ok(Some(path.display().to_string()))
    })
    .await?;

    match outcome {
        Ok(Some(path)) => Ok(Json(json!({
            "command": command.as_str(),
            "control_path": path,
        }))
        .into_response()),
        Ok(None) => Ok((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no such job" })),
        )
            .into_response()),
        Err(error) => Ok(bad_request(&error.to_string())),
    }
}

async fn dashboard_options() -> Json<DashboardOptions> {
    Json(dashboard_options_payload())
}

/// Launch a new translation from an uploaded EPUB.
///
/// Runs the translation as a detached `bookforge translate` subprocess. The
/// child inherits this process's environment and may receive a dashboard-supplied
/// session key through the provider's normal key env var; key values are never
/// placed on the command line. The job is matched back to the dashboard by its
/// unique input path (returned to the client), since the run generates its own
/// job id.
async fn launch_translate(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<Response, AppError> {
    if let Some(response) = reject_mutation(&headers, &state) {
        return Ok(response);
    }

    let mut file_bytes: Option<Vec<u8>> = None;
    let mut file_name = "upload.epub".to_string();
    let mut fields: HashMap<String, String> = HashMap::new();

    while let Some(field) = multipart.next_field().await? {
        let name = field.name().unwrap_or_default().to_string();
        if name == "file" {
            if let Some(fname) = field.file_name()
                && !fname.is_empty()
            {
                file_name = fname.to_string();
            }
            file_bytes = Some(field.bytes().await?.to_vec());
        } else {
            fields.insert(name, field.text().await?);
        }
    }

    let Some(bytes) = file_bytes.filter(|b| !b.is_empty()) else {
        return Ok(bad_request("upload an EPUB file"));
    };
    let Some(target) = field_value(&fields, "target") else {
        return Ok(bad_request("target language is required"));
    };
    let provider = field_value(&fields, "provider").unwrap_or_else(|| "mock".to_string());
    if !supported_provider(&provider) {
        return Ok(bad_request("unsupported provider"));
    }
    let openai_base_url = if provider == "openai-compatible" {
        let Some(base_url) = field_value(&fields, "base_url") else {
            return Ok(bad_request("base URL is required for openai-compatible"));
        };
        if !dashboard_base_url_uses_https(&base_url) {
            return Ok(bad_request(
                "base URL must use https:// for openai-compatible",
            ));
        }
        if field_value(&fields, "model").is_none() {
            return Ok(bad_request("model is required for openai-compatible"));
        }
        Some(base_url)
    } else {
        None
    };

    // Resolve the API key: a freshly-supplied one is remembered for the session;
    // otherwise reuse one already remembered for this provider. A blank key falls
    // through to the run's normal environment-variable resolution.
    let supplied_key = (provider != "mock")
        .then(|| field_value(&fields, "api_key"))
        .flatten();
    let key = if let Some(supplied) = supplied_key {
        lock_keys(&state)?.insert(provider.clone(), supplied.clone());
        Some(supplied)
    } else {
        lock_keys(&state)?.get(&provider).cloned()
    };
    if provider != "mock" && key.is_none() && !provider_env_has_key(&provider) {
        return Ok(bad_request("provider API key is required"));
    }

    let stem = sanitize_component(strip_epub_suffix(&file_name));
    let tag = format!("{}-{stem}", now_ms());
    let upload_dir = PathBuf::from(UPLOAD_DIR);
    std::fs::create_dir_all(&upload_dir)?;
    let input_path = upload_dir.join(format!("{tag}.epub"));
    std::fs::write(&input_path, &bytes)?;
    let out_path = upload_dir.join(format!("{tag}.{}.epub", sanitize_component(&target)));

    let exe = std::env::current_exe()?;
    let mut command = tokio::process::Command::new(exe);
    command
        .arg("translate")
        .arg(&input_path)
        .arg("--target")
        .arg(&target)
        .arg("--provider")
        .arg(&provider)
        .arg("--ui")
        .arg("quiet")
        .arg("--out")
        .arg(&out_path);
    if let Some(source) = field_value(&fields, "source") {
        command.arg("--source").arg(source);
    }
    // Offline mock runs are identity translations unless told otherwise.
    let model = field_value(&fields, "model")
        .or_else(|| (provider == "mock").then(|| "mock-identity".to_string()));
    if let Some(model) = model {
        command.arg("--model").arg(model);
    }
    if let Some(profile) = field_value(&fields, "profile") {
        command.arg("--profile").arg(profile);
    }
    // Advanced tuning from the wizard, each validated before forwarding so the
    // child never receives arbitrary argv from the browser.
    if let Some(concurrency) = field_value(&fields, "concurrency")
        .and_then(|value| value.parse::<usize>().ok())
        .map(|value| value.clamp(1, 16))
    {
        command.arg("--concurrency").arg(concurrency.to_string());
    }
    if let Some(context) = field_value(&fields, "context_window")
        .and_then(|value| value.parse::<usize>().ok())
        .map(|value| value.min(16))
    {
        command.arg("--context-window").arg(context.to_string());
    }
    if let Some(qa) = field_value(&fields, "qa")
        .filter(|value| matches!(value.as_str(), "off" | "suspicious" | "all"))
    {
        command.arg("--qa").arg(qa);
    }
    if field_value(&fields, "validate_output")
        .is_some_and(|value| matches!(value.as_str(), "true" | "on" | "1"))
    {
        command.arg("--validate-output");
    }
    if let Some(base_url) = openai_base_url {
        command.arg("--base-url").arg(base_url);
    }
    // Inject the key through the environment (never argv), pointing the run at a
    // canonical provider env var. The child records that env-var name in its job
    // snapshot, so `bookforge resume` can use the same env name later.
    if provider != "mock"
        && let Some(key) = key
    {
        let env = provider_key_env(&provider).expect("provider was validated");
        command.arg("--api-key-env").arg(env);
        command.env(env, key);
    }

    // Detached: the run outlives this request. The short startup check catches
    // immediate argv/binary failures before the dashboard reports success.
    let mut child = command
        .spawn()
        .context("failed to spawn translation process")?;
    let pid = child.id();
    let completed_immediately = if let Some(status) =
        child_exit_status_after(&mut child, CHILD_STARTUP_CHECK).await?
    {
        if !status.success() {
            return Err(anyhow::anyhow!(
                    "translation process exited immediately with {status}; check the serve console for details"
                )
                .into());
        }
        true
    } else {
        false
    };

    Ok(Json(json!({
        "ok": true,
        "input_path": input_path.display().to_string(),
        "provider": provider,
        "pid": pid,
        "completed_immediately": completed_immediately,
    }))
    .into_response())
}

async fn child_exit_status_after(
    child: &mut tokio::process::Child,
    delay: Duration,
) -> Result<Option<ExitStatus>> {
    tokio::time::sleep(delay).await;
    child
        .try_wait()
        .context("failed to check translation process status")
}

/// Estimate tokens and cost for an uploaded EPUB before the user commits to a
/// run. Shares [`estimate_epub`](super::estimate::estimate_epub) with the CLI
/// `estimate` command. The upload is written to a temp file (EPUB parsing reads
/// from disk) and removed immediately after.
async fn estimate_translate(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<Response, AppError> {
    if let Some(response) = reject_mutation(&headers, &state) {
        return Ok(response);
    }

    let mut file_bytes: Option<Vec<u8>> = None;
    let mut fields: HashMap<String, String> = HashMap::new();
    while let Some(field) = multipart.next_field().await? {
        let name = field.name().unwrap_or_default().to_string();
        if name == "file" {
            file_bytes = Some(field.bytes().await?.to_vec());
        } else {
            fields.insert(name, field.text().await?);
        }
    }

    let Some(bytes) = file_bytes.filter(|b| !b.is_empty()) else {
        return Ok(bad_request("upload an EPUB file"));
    };
    let provider = field_value(&fields, "provider").unwrap_or_else(|| "mock".to_string());
    if !supported_provider(&provider) {
        return Ok(bad_request("unsupported provider"));
    }
    let model = field_value(&fields, "model");

    let result = tokio::task::spawn_blocking(move || {
        let seq = ESTIMATE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "bookforge-estimate-{}-{}-{}.epub",
            std::process::id(),
            now_ms(),
            seq
        ));
        std::fs::write(&path, &bytes)?;
        let outcome = super::estimate::estimate_epub(&path, &provider, model.as_deref(), None);
        let _ = std::fs::remove_file(&path);
        outcome
    })
    .await?;

    match result {
        Ok(est) => Ok(Json(json!({
            "segments": est.segments,
            "input_tokens": est.input_tokens,
            "output_tokens": est.output_tokens,
            "model": est.model,
            "cost_usd": est.cost_usd,
        }))
        .into_response()),
        Err(err) => Ok(bad_request(&format!("could not estimate: {err}"))),
    }
}

/// Report which providers already have a usable key — either remembered in this
/// session or present in the server's environment — so the UI only prompts when
/// a key is actually needed. Never returns key material.
async fn provider_status(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, AppError> {
    let remembered = lock_keys(&state)?;
    let mut status = serde_json::Map::new();
    for (provider, env) in PROVIDER_KEY_ENVS {
        let configured = remembered.contains_key(*provider)
            || std::env::var(env).map(|v| !v.is_empty()).unwrap_or(false);
        status.insert((*provider).to_string(), json!(configured));
    }
    Ok(Json(serde_json::Value::Object(status)))
}

// ---------------------------------------------------------------------------
// Glossary (wraps the `glossary` command's JobStore methods)
// ---------------------------------------------------------------------------

fn parse_glossary_scope(value: &str) -> GlossaryScopeKind {
    match value {
        "series" => GlossaryScopeKind::Series,
        "book" => GlossaryScopeKind::Book,
        _ => GlossaryScopeKind::Global,
    }
}

fn parse_glossary_category(value: &str) -> GlossaryCategory {
    match value {
        "person" => GlossaryCategory::Person,
        "place" => GlossaryCategory::Place,
        "object" => GlossaryCategory::Object,
        "invented" => GlossaryCategory::Invented,
        "style" => GlossaryCategory::Style,
        "phrase" => GlossaryCategory::Phrase,
        _ => GlossaryCategory::Other,
    }
}

fn glossary_term_json(term: &GlossaryTerm) -> serde_json::Value {
    json!({
        "id": term.id,
        "source": term.source_text,
        "target": term.target_text,
        "category": term.category.as_str(),
        "scope": term.scope_kind.as_str(),
        "scope_id": term.scope_id,
        "source_language": term.source_language,
        "target_language": term.target_language,
        "always_active": term.always_active,
        "case_sensitive": term.case_sensitive,
        "notes": term.notes,
    })
}

/// List glossary terms, optionally filtered by `source`/`target` language,
/// `scope` (global/series/book) and `scope_id`.
async fn list_glossary(
    Query(params): Query<HashMap<String, String>>,
    State(state): State<AppState>,
) -> Result<Json<Vec<serde_json::Value>>, AppError> {
    let store_path = state.store_path.clone();
    let items = tokio::task::spawn_blocking(move || -> Result<Vec<serde_json::Value>> {
        let store = JobStore::open(store_path)?;
        let terms = store.list_glossary_terms(GlossaryFilter {
            scope_kind: params.get("scope").map(|value| parse_glossary_scope(value)),
            scope_id: params
                .get("scope_id")
                .map(String::as_str)
                .filter(|value| !value.is_empty()),
            source_language: params
                .get("source")
                .map(String::as_str)
                .filter(|value| !value.is_empty()),
            target_language: params
                .get("target")
                .map(String::as_str)
                .filter(|value| !value.is_empty()),
            active_only: false,
        })?;
        Ok(terms.iter().map(glossary_term_json).collect())
    })
    .await??;
    Ok(Json(items))
}

#[derive(Deserialize)]
struct GlossaryAddRequest {
    source: String,
    target: String,
    source_language: String,
    target_language: String,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    scope_id: Option<String>,
    #[serde(default)]
    notes: Option<String>,
    #[serde(default)]
    case_sensitive: bool,
    #[serde(default)]
    always_active: bool,
}

async fn add_glossary(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<GlossaryAddRequest>,
) -> Result<Response, AppError> {
    if let Some(response) = reject_mutation(&headers, &state) {
        return Ok(response);
    }
    if req.source.trim().is_empty() || req.target.trim().is_empty() {
        return Ok(bad_request("source term and translation are required"));
    }
    if req.source_language.trim().is_empty() || req.target_language.trim().is_empty() {
        return Ok(bad_request("source and target languages are required"));
    }
    let scope_kind = req
        .scope
        .as_deref()
        .map(parse_glossary_scope)
        .unwrap_or(GlossaryScopeKind::Global);
    let scope_id = if scope_kind == GlossaryScopeKind::Global {
        None
    } else {
        req.scope_id.filter(|value| !value.trim().is_empty())
    };
    if scope_kind != GlossaryScopeKind::Global && scope_id.is_none() {
        return Ok(bad_request("scope_id is required for series/book scope"));
    }

    let term = GlossaryTerm {
        id: None,
        scope_kind,
        scope_id,
        source_text: req.source.trim().to_string(),
        target_text: req.target.trim().to_string(),
        category: req
            .category
            .as_deref()
            .map(parse_glossary_category)
            .unwrap_or(GlossaryCategory::Other),
        notes: req.notes.filter(|value| !value.trim().is_empty()),
        case_sensitive: req.case_sensitive,
        always_active: req.always_active,
        status: GlossaryStatus::UserSeeded,
        source_language: req.source_language.trim().to_string(),
        target_language: req.target_language.trim().to_string(),
        source_count: 0,
    };

    let store_path = state.store_path.clone();
    let id = tokio::task::spawn_blocking(move || -> Result<i64> {
        let store = JobStore::open(store_path)?;
        Ok(store.add_glossary_term(&term)?)
    })
    .await??;
    Ok(Json(json!({ "id": id })).into_response())
}

async fn remove_glossary(
    AxumPath(id): AxumPath<i64>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    if let Some(response) = reject_mutation(&headers, &state) {
        return Ok(response);
    }
    let store_path = state.store_path.clone();
    let removed = tokio::task::spawn_blocking(move || -> Result<usize> {
        let store = JobStore::open(store_path)?;
        Ok(store.remove_glossary_term(id)?)
    })
    .await??;
    Ok(Json(json!({ "removed": removed })).into_response())
}

fn supported_provider(provider: &str) -> bool {
    provider == "mock" || provider_key_env(provider).is_some()
}

fn dashboard_base_url_uses_https(base_url: &str) -> bool {
    reqwest::Url::parse(base_url).is_ok_and(|url| url.scheme() == "https" && url.host().is_some())
}

fn provider_key_env(provider: &str) -> Option<&'static str> {
    PROVIDER_KEY_ENVS
        .iter()
        .find_map(|(known, env)| (*known == provider).then_some(*env))
}

fn provider_env_has_key(provider: &str) -> bool {
    provider_key_env(provider)
        .and_then(|env| std::env::var(env).ok())
        .is_some_and(|value| !value.is_empty())
}

fn lock_keys(state: &AppState) -> Result<MutexGuard<'_, HashMap<String, String>>> {
    state
        .keys
        .lock()
        .map_err(|_| anyhow::anyhow!("dashboard API key store is unavailable"))
}

fn dashboard_options_payload() -> DashboardOptions {
    DashboardOptions {
        languages: LANGUAGE_OPTIONS,
        providers: vec![
            ProviderOption {
                id: "mock",
                label: "mock (offline test)",
                models: MOCK_MODELS,
                default_model: "mock-identity",
                requires_base_url: false,
                requires_key: false,
            },
            ProviderOption {
                id: "deepseek",
                label: "deepseek",
                models: DEEPSEEK_MODELS,
                default_model: "deepseek-v4-flash",
                requires_base_url: false,
                requires_key: true,
            },
            ProviderOption {
                id: "openrouter",
                label: "openrouter",
                models: OPENROUTER_MODELS,
                default_model: "openrouter/auto",
                requires_base_url: false,
                requires_key: true,
            },
            ProviderOption {
                id: "openai-compatible",
                label: "openai-compatible",
                models: OPENAI_COMPATIBLE_MODELS,
                default_model: "gpt-4o-mini",
                requires_base_url: true,
                requires_key: true,
            },
        ],
    }
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

fn reject_mutation(headers: &HeaderMap, state: &AppState) -> Option<Response> {
    if is_cross_site_browser_request(headers) {
        return Some(forbidden("cross-site dashboard request rejected"));
    }

    match headers
        .get(CSRF_HEADER)
        .and_then(|value| value.to_str().ok())
    {
        Some(token) if constant_time_eq(token.as_bytes(), state.csrf_token.as_bytes()) => None,
        _ => Some(forbidden("missing or invalid dashboard token")),
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }

    let mut diff = 0u8;
    for (left, right) in left.iter().zip(right.iter()) {
        diff |= left ^ right;
    }
    diff == 0
}

fn is_cross_site_browser_request(headers: &HeaderMap) -> bool {
    headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|site| {
            site.eq_ignore_ascii_case("cross-site")
                || site.eq_ignore_ascii_case("same-site")
                || site.eq_ignore_ascii_case("none") && has_nonlocal_origin(headers)
        })
}

fn has_nonlocal_origin(headers: &HeaderMap) -> bool {
    headers
        .get("origin")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|origin| {
            !(origin.starts_with("http://127.0.0.1:")
                || origin.starts_with("http://localhost:")
                || origin.starts_with("http://[::1]:"))
        })
}

fn generate_csrf_token() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).context("failed to generate dashboard token")?;
    Ok(hex_bytes(&bytes))
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut out, "{byte:02x}").expect("writing to string cannot fail");
    }
    out
}

/// Resolve a job's event-log path off the async runtime (sqlite is blocking).
async fn resolve_events_path(id: String, store_path: PathBuf) -> PathBuf {
    let fallback = PathBuf::from(format!(".bookforge/runs/{id}/events.jsonl"));
    let lookup = id.clone();
    tokio::task::spawn_blocking(move || {
        let job = JobStore::open(store_path)
            .ok()
            .and_then(|store| store.get_job(&lookup).ok().flatten());
        events_path_for(job.as_ref(), &lookup)
    })
    .await
    .unwrap_or(fallback)
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct JobListItem {
    id: String,
    status: String,
    provider: String,
    model: String,
    target_lang: String,
    input_path: String,
    total_segments: usize,
    done: usize,
    succeeded: usize,
    cached: usize,
    needs_review: usize,
    failed: usize,
    retry_pending: usize,
    input_tokens: u64,
    output_tokens: u64,
}

impl JobListItem {
    fn new(job: &JobRecord, summary: &JobSummary) -> Self {
        let done = summary.succeeded + summary.cached + summary.needs_review + summary.failed;
        Self {
            id: job.id.clone(),
            status: summary.status.clone(),
            provider: job.provider.clone(),
            model: job.model.clone(),
            target_lang: job.target_lang.clone(),
            input_path: job.input_path.display().to_string(),
            total_segments: summary.total_segments,
            done,
            succeeded: summary.succeeded,
            cached: summary.cached,
            needs_review: summary.needs_review,
            failed: summary.failed,
            retry_pending: summary.retry_pending,
            input_tokens: summary.input_tokens,
            output_tokens: summary.output_tokens,
        }
    }
}

#[derive(Serialize)]
struct JobDetail {
    id: String,
    input_path: String,
    output_path: String,
    provider: String,
    model: String,
    source_lang: Option<String>,
    target_lang: String,
    status: String,
    state: RunState,
}

#[derive(Debug, Serialize)]
struct RuntimeMutableSettings {
    batch_max_output_tokens: Option<u32>,
    batch_max_items: usize,
    batch_target_tokens: usize,
    concurrency: usize,
    qa: crate::QaMode,
    double_check: bookforge_core::DoubleCheckMode,
    validate_output: bool,
    provider_max_attempts: usize,
    adaptive_concurrency: bool,
    adaptive_batch_sizing: bool,
}

#[derive(Debug, Serialize)]
struct RuntimeIdentity {
    provider: String,
    model: String,
    source_language: Option<String>,
    target_language: String,
    profile: String,
    prompt_version: String,
}

#[derive(Debug, Serialize)]
struct RuntimeLeaseView {
    state: &'static str,
    pid: Option<u32>,
    instance_id: Option<String>,
    heartbeat_at_ms: Option<u64>,
    last_loaded_revision: Option<u64>,
    last_applied_revision: Option<u64>,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct RuntimeSettingsView {
    effective: RuntimeMutableSettings,
    overrides: super::reconfigure::RunConfigOverrides,
    revision: u64,
    applied_revision: u64,
    changed_fields: Vec<String>,
    next_boundary: Vec<String>,
    application_state: &'static str,
    live: bool,
    editable: bool,
    resumable_work: bool,
    lease: RuntimeLeaseView,
    identity: RuntimeIdentity,
}

fn runtime_settings_view(
    store_path: &std::path::Path,
    id: &str,
) -> Result<Option<RuntimeSettingsView>> {
    let store = JobStore::open(store_path)?;
    let Some(job) = store.get_job(id)? else {
        return Ok(None);
    };
    let Some(snapshot) = store.load_job_config_snapshot(id)? else {
        return Ok(None);
    };
    let mut settings = snapshot.settings.to_settings();
    let loaded = super::reconfigure::load_overrides_document_for_job(id)?;
    let (revision, overrides) = loaded
        .map(|loaded| (loaded.revision, loaded.overrides))
        .unwrap_or_default();
    super::reconfigure::apply_overrides_to_settings(&mut settings, &overrides);
    let qa = overrides
        .qa
        .unwrap_or_else(|| crate::QaMode::from_snapshot(&snapshot.qa_mode));
    let validate_output = overrides
        .validate_output
        .unwrap_or(snapshot.validate_output);
    let changed_fields = overrides.changed_fields();
    let next_boundary = overrides.application_boundaries();
    let (lease, live, applied_revision) = match crate::control::runtime_lease_state(id) {
        crate::control::RuntimeLeaseState::Fresh(lease) => (
            RuntimeLeaseView {
                state: "fresh",
                pid: Some(lease.pid),
                instance_id: Some(lease.instance_id.clone()),
                heartbeat_at_ms: Some(lease.heartbeat_at_ms),
                last_loaded_revision: Some(lease.last_loaded_revision),
                last_applied_revision: Some(lease.last_applied_revision),
                error: None,
            },
            true,
            lease.last_applied_revision,
        ),
        crate::control::RuntimeLeaseState::Stale(lease) => (
            RuntimeLeaseView {
                state: "stale",
                pid: Some(lease.pid),
                instance_id: Some(lease.instance_id.clone()),
                heartbeat_at_ms: Some(lease.heartbeat_at_ms),
                last_loaded_revision: Some(lease.last_loaded_revision),
                last_applied_revision: Some(lease.last_applied_revision),
                error: None,
            },
            false,
            lease.last_applied_revision,
        ),
        crate::control::RuntimeLeaseState::Missing => (
            RuntimeLeaseView {
                state: "missing",
                pid: None,
                instance_id: None,
                heartbeat_at_ms: None,
                last_loaded_revision: None,
                last_applied_revision: None,
                error: None,
            },
            false,
            0,
        ),
        crate::control::RuntimeLeaseState::Invalid(error) => (
            RuntimeLeaseView {
                state: "invalid",
                pid: None,
                instance_id: None,
                heartbeat_at_ms: None,
                last_loaded_revision: None,
                last_applied_revision: None,
                error: Some(error),
            },
            false,
            0,
        ),
    };
    // Translation is only one part of the resumable pipeline. A stopped,
    // paused, or orphaned-running job may have no pending segments while QA,
    // double-check, rebuild, validation, or reporting still remains.
    let resumable_work = !store.resumable_segment_ids(id)?.is_empty()
        || job_status_has_unfinished_pipeline_work(&job.status);
    let editable = job_status_has_unfinished_pipeline_work(&job.status) && resumable_work;
    let application_state = if !live {
        "resume_required"
    } else if revision > applied_revision {
        "next_boundary"
    } else {
        "live"
    };
    Ok(Some(RuntimeSettingsView {
        effective: RuntimeMutableSettings {
            batch_max_output_tokens: settings.provider.batch_max_output_tokens,
            batch_max_items: settings.batch.max_items,
            batch_target_tokens: settings.batch.target_tokens,
            concurrency: settings.scheduler.concurrency,
            qa,
            double_check: settings.double_check.mode,
            validate_output,
            provider_max_attempts: settings.provider.provider_max_attempts,
            adaptive_concurrency: settings.adaptive_concurrency,
            adaptive_batch_sizing: settings.batch.adaptive_sizing,
        },
        overrides,
        revision,
        applied_revision,
        changed_fields,
        next_boundary,
        application_state,
        live,
        editable,
        resumable_work,
        lease,
        identity: RuntimeIdentity {
            provider: snapshot.provider,
            model: snapshot.model,
            source_language: snapshot.source_language,
            target_language: snapshot.target_language,
            profile: format!("{:?}", snapshot.profile),
            prompt_version: snapshot.prompt_version,
        },
    }))
}

#[derive(Serialize)]
struct DashboardOptions {
    languages: &'static [&'static str],
    providers: Vec<ProviderOption>,
}

#[derive(Serialize)]
struct ProviderOption {
    id: &'static str,
    label: &'static str,
    models: &'static [&'static str],
    default_model: &'static str,
    requires_base_url: bool,
    requires_key: bool,
}

impl JobDetail {
    fn new(id: String, job: Option<JobRecord>, state: RunState) -> Self {
        let provider = job
            .as_ref()
            .map(|j| j.provider.clone())
            .or_else(|| state.provider.clone())
            .unwrap_or_default();
        let model = job
            .as_ref()
            .map(|j| j.model.clone())
            .or_else(|| state.model.clone())
            .unwrap_or_default();
        let input_path = job
            .as_ref()
            .map(|j| j.input_path.display().to_string())
            .or_else(|| state.input_path.clone())
            .unwrap_or_default();
        let output_path = job
            .as_ref()
            .map(|j| j.output_path.display().to_string())
            .or_else(|| state.output_path.clone())
            .unwrap_or_default();
        Self {
            id,
            input_path,
            output_path,
            provider,
            model,
            source_lang: job.as_ref().and_then(|j| j.source_lang.clone()),
            target_lang: job
                .as_ref()
                .map(|j| j.target_lang.clone())
                .unwrap_or_default(),
            status: job.as_ref().map(|j| j.status.clone()).unwrap_or_else(|| {
                if state.finished {
                    "done".into()
                } else if state.paused {
                    "paused".into()
                } else {
                    "running".into()
                }
            }),
            state,
        }
    }
}

// ---------------------------------------------------------------------------
// Error plumbing
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Inline frontend (no build step — vanilla JS, mirrors the review.rs pattern)
// ---------------------------------------------------------------------------

const DASHBOARD_HTML_TEMPLATE: &str = include_str!("serve/dashboard.html");
const DASHBOARD_CSS: &str = include_str!("serve/dashboard.css");
const DASHBOARD_JS: &str = include_str!("serve/dashboard.js");

fn assemble_dashboard_html(template: &str, css: &str, js: &str) -> String {
    let template = template.replace("\r\n", "\n");
    let css = css.replace("\r\n", "\n");
    let js = js.replace("\r\n", "\n");

    template
        .replace("{{BOOKFORGE_DASHBOARD_CSS}}", &css)
        .replace("{{BOOKFORGE_DASHBOARD_JS}}", &js)
}

static DASHBOARD_HTML: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    assemble_dashboard_html(DASHBOARD_HTML_TEMPLATE, DASHBOARD_CSS, DASHBOARD_JS)
});

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    const TEST_HOST: &str = "127.0.0.1:8765";

    fn test_state(token: &str) -> AppState {
        AppState {
            refresh: Duration::from_millis(250),
            csrf_token: token.to_string(),
            host_port: 8765,
            keys: Arc::new(Mutex::new(HashMap::new())),
            store_path: default_store_path(),
            resume_launches: None,
        }
    }

    /// Like [`test_state`], but pointed at an isolated store path instead of
    /// the process-relative default — lets tests exercise the store-backed
    /// mutation endpoints against a temp-dir database without chdir'ing the
    /// (shared, per-process) current directory, which would race across
    /// parallel test threads.
    fn test_state_with_store(token: &str, store_path: PathBuf) -> AppState {
        AppState {
            store_path,
            ..test_state(token)
        }
    }

    #[test]
    fn bind_must_be_loopback() {
        let local: SocketAddr = "127.0.0.1:8765".parse().unwrap();
        let remote: SocketAddr = "0.0.0.0:8765".parse().unwrap();

        assert!(require_loopback_bind(local).is_ok());
        assert!(require_loopback_bind(remote).is_err());
    }

    #[test]
    fn dashboard_host_header_allows_only_loopback_names_on_bound_port() {
        for host in [
            "127.0.0.1:8765",
            "localhost:8765",
            "LOCALHOST:8765",
            "[::1]:8765",
        ] {
            assert!(
                dashboard_host_value_allowed(host, 8765),
                "{host} should be allowed"
            );
        }

        for host in [
            "",
            "127.0.0.1",
            "127.0.0.1:8766",
            " 127.0.0.1:8765",
            "127.0.0.1:8765 ",
            "127.0.0.1.evil.test:8765",
            "localhost.evil.test:8765",
            "evil.test:8765",
            "127.0.0.1:8765.evil.test",
            "127.0.0.1:08765",
            "[::1]:8766",
            "[::ffff:127.0.0.1]:8765",
        ] {
            assert!(
                !dashboard_host_value_allowed(host, 8765),
                "{host} should be rejected"
            );
        }
    }

    #[test]
    fn provider_key_envs_cover_supported_cloud_providers() {
        assert_eq!(provider_key_env("deepseek"), Some("DEEPSEEK_API_KEY"));
        assert_eq!(provider_key_env("openrouter"), Some("OPENROUTER_API_KEY"));
        assert_eq!(
            provider_key_env("openai-compatible"),
            Some("OPENAI_API_KEY")
        );
        assert!(provider_key_env("mock").is_none());
    }

    #[test]
    fn dashboard_options_include_common_languages_and_models() {
        let options = dashboard_options_payload();
        assert!(options.languages.contains(&"Italian"));
        assert!(options.languages.contains(&"English"));

        let deepseek = options
            .providers
            .iter()
            .find(|provider| provider.id == "deepseek")
            .expect("deepseek provider option should exist");
        assert_eq!(deepseek.default_model, "deepseek-v4-flash");
        assert!(deepseek.models.contains(&"deepseek-v4-pro"));

        let openrouter = options
            .providers
            .iter()
            .find(|provider| provider.id == "openrouter")
            .expect("openrouter provider option should exist");
        assert!(openrouter.models.contains(&"google/gemini-2.5-flash"));
    }

    #[test]
    fn dashboard_escapes_dynamic_html_fields() {
        assert!(DASHBOARD_HTML.contains("function esc(value)"));
        assert!(DASHBOARD_HTML.contains("${esc(d.id)}"));
        assert!(DASHBOARD_HTML.contains("${esc(body)}"));
    }

    #[test]
    fn dashboard_openai_compatible_base_url_must_be_https() {
        assert!(dashboard_base_url_uses_https("https://api.example.com/v1"));
        assert!(!dashboard_base_url_uses_https("http://api.example.com/v1"));
        assert!(!dashboard_base_url_uses_https("https://"));
        assert!(!dashboard_base_url_uses_https("not a url"));
    }

    #[tokio::test]
    async fn child_startup_check_reports_immediate_success_exit() -> Result<()> {
        let mut child = tokio::process::Command::new(std::env::current_exe()?)
            .arg("--help")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;

        let status = child_exit_status_after(&mut child, Duration::from_secs(2))
            .await?
            .expect("help child should exit quickly");

        assert!(status.success());
        Ok(())
    }

    #[test]
    fn mutating_routes_require_dashboard_token() {
        let state = test_state("token-123");
        let headers = HeaderMap::new();
        assert!(reject_mutation(&headers, &state).is_some());

        let mut headers = HeaderMap::new();
        headers.insert(CSRF_HEADER, HeaderValue::from_static("token-123"));
        assert!(reject_mutation(&headers, &state).is_none());
    }

    #[test]
    fn mutating_routes_reject_cross_site_browser_requests() {
        let state = test_state("token-123");
        let mut headers = HeaderMap::new();
        headers.insert(CSRF_HEADER, HeaderValue::from_static("token-123"));
        headers.insert("sec-fetch-site", HeaderValue::from_static("cross-site"));

        assert!(reject_mutation(&headers, &state).is_some());
    }

    #[test]
    fn csrf_token_compare_matches_only_exact_token() {
        assert!(constant_time_eq(b"token-123", b"token-123"));
        assert!(!constant_time_eq(b"token-123", b"token-124"));
        assert!(!constant_time_eq(b"token-123", b"token-1234"));
    }

    #[tokio::test]
    async fn dashboard_rejects_untrusted_host_header_before_serving_html() {
        use axum::{body::Body, http::Request};
        use tower::ServiceExt;

        let response = dashboard_router(test_state("token-123"))
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header("host", "evil.test:8765")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("route should respond");

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn dashboard_serves_loopback_host_header() {
        use axum::{body::Body, http::Request};
        use tower::ServiceExt;

        let response = dashboard_router(test_state("token-123"))
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header("host", "localhost:8765")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("route should respond");

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn retry_endpoint_rejects_missing_dashboard_token() {
        use axum::{body::Body, http::Request};
        use tower::ServiceExt;

        let response = dashboard_router(test_state("token-123"))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/jobs/not-real/retry")
                    .header("host", TEST_HOST)
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("route should respond");

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn control_endpoints_reject_missing_dashboard_token() {
        use axum::{body::Body, http::Request};
        use tower::ServiceExt;

        let router = dashboard_router(test_state("token-123"));
        for command in ["pause", "resume", "stop"] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(format!("/api/jobs/not-real/{command}"))
                        .header("host", TEST_HOST)
                        .body(Body::empty())
                        .expect("request should build"),
                )
                .await
                .expect("route should respond");

            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{command}");
        }
    }

    #[tokio::test]
    async fn estimate_endpoint_rejects_missing_dashboard_token() {
        use axum::{body::Body, http::Request};
        use tower::ServiceExt;

        // A well-formed multipart body so the Multipart extractor succeeds and
        // the handler's own CSRF check is what rejects the request.
        let body =
            "--B\r\nContent-Disposition: form-data; name=\"provider\"\r\n\r\nmock\r\n--B--\r\n";
        let response = dashboard_router(test_state("token-123"))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/estimate")
                    .header("host", TEST_HOST)
                    .header("content-type", "multipart/form-data; boundary=B")
                    .body(Body::from(body))
                    .expect("request should build"),
            )
            .await
            .expect("route should respond");

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn glossary_mutations_reject_missing_dashboard_token() {
        use axum::{body::Body, http::Request};
        use tower::ServiceExt;

        let add = dashboard_router(test_state("token-123"))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/glossary")
                    .header("host", TEST_HOST)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"source":"a","target":"b","source_language":"English","target_language":"Italian"}"#,
                    ))
                    .expect("request should build"),
            )
            .await
            .expect("route should respond");
        assert_eq!(add.status(), StatusCode::FORBIDDEN);

        let remove = dashboard_router(test_state("token-123"))
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/glossary/1")
                    .header("host", TEST_HOST)
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("route should respond");
        assert_eq!(remove.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn translate_rejects_non_https_openai_compatible_base_url() {
        use axum::{body::Body, http::Request};
        use tower::ServiceExt;

        let body = concat!(
            "--B\r\n",
            "Content-Disposition: form-data; name=\"file\"; filename=\"book.epub\"\r\n",
            "Content-Type: application/epub+zip\r\n\r\n",
            "not-a-real-epub\r\n",
            "--B\r\n",
            "Content-Disposition: form-data; name=\"target\"\r\n\r\n",
            "Italian\r\n",
            "--B\r\n",
            "Content-Disposition: form-data; name=\"provider\"\r\n\r\n",
            "openai-compatible\r\n",
            "--B\r\n",
            "Content-Disposition: form-data; name=\"base_url\"\r\n\r\n",
            "http://api.example.test/v1\r\n",
            "--B\r\n",
            "Content-Disposition: form-data; name=\"model\"\r\n\r\n",
            "test-model\r\n",
            "--B\r\n",
            "Content-Disposition: form-data; name=\"api_key\"\r\n\r\n",
            "sk-test-key\r\n",
            "--B--\r\n",
        );

        let response = dashboard_router(test_state("token-123"))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/translate")
                    .header("host", TEST_HOST)
                    .header(CSRF_HEADER, "token-123")
                    .header("content-type", "multipart/form-data; boundary=B")
                    .body(Body::from(body))
                    .expect("request should build"),
            )
            .await
            .expect("route should respond");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn glossary_scope_and_category_parse_with_sane_defaults() {
        assert_eq!(parse_glossary_scope("book"), GlossaryScopeKind::Book);
        assert_eq!(parse_glossary_scope("series"), GlossaryScopeKind::Series);
        assert_eq!(parse_glossary_scope("whatever"), GlossaryScopeKind::Global);
        assert_eq!(parse_glossary_category("place"), GlossaryCategory::Place);
        assert_eq!(parse_glossary_category("mystery"), GlossaryCategory::Other);
    }

    #[test]
    fn dashboard_ships_all_screen_renderers() {
        for marker in [
            "function renderLibrary",
            "function renderWizard",
            "function renderProgress",
            "function drawReview",
            "function drawValidation",
            "function renderGlossary",
        ] {
            assert!(DASHBOARD_HTML.contains(marker), "missing {marker}");
        }
    }

    #[test]
    fn dashboard_assets_reassemble_byte_stably() {
        use sha2::{Digest, Sha256};

        assert_eq!(DASHBOARD_HTML.len(), 82_407);
        assert!(!DASHBOARD_HTML.contains("{{BOOKFORGE_DASHBOARD_CSS}}"));
        assert!(!DASHBOARD_HTML.contains("{{BOOKFORGE_DASHBOARD_JS}}"));
        assert_eq!(
            format!("{:x}", Sha256::digest(DASHBOARD_HTML.as_bytes())),
            "7a37e7095182825d2f63afec9776214ce7f99ea33464ad1e86ea43342767ce9b"
        );

        let crlf = |asset: &str| asset.replace("\r\n", "\n").replace('\n', "\r\n");
        assert_eq!(
            assemble_dashboard_html(
                &crlf(DASHBOARD_HTML_TEMPLATE),
                &crlf(DASHBOARD_CSS),
                &crlf(DASHBOARD_JS),
            ),
            *DASHBOARD_HTML,
        );
    }

    #[test]
    fn dashboard_ships_runtime_editor_and_inline_retry_guidance() {
        for marker in [
            "function drawRuntimeSettings",
            "function bfSaveRuntimeSettings",
            "RuntimeConfigChanged",
            "function bfReviewRetrySubmit",
            "Stop the job before queuing a retry",
            "function bfReviewStopForRetry",
        ] {
            assert!(DASHBOARD_HTML.contains(marker), "missing {marker}");
        }
        assert!(
            !DASHBOARD_HTML.contains("window.prompt"),
            "retry guidance must use the inline editor"
        );
    }

    #[test]
    fn dashboard_posts_include_csrf_header() {
        assert!(DASHBOARD_HTML.contains(CSRF_TOKEN_PLACEHOLDER));
        assert!(DASHBOARD_HTML.contains("headers: { [CSRF_HEADER]: CSRF_TOKEN }"));
    }

    #[test]
    fn csrf_token_is_hex_encoded_random_bytes() {
        let token = generate_csrf_token().expect("token should generate");
        assert_eq!(token.len(), 32);
        assert!(token.chars().all(|ch| ch.is_ascii_hexdigit()));
    }

    // -----------------------------------------------------------------------
    // Segment mutation endpoints: CSRF rejection + isolated-store end-to-end.
    //
    // These build a real completed job (real EPUB parsed with `read_epub`,
    // real `Segment`s from `build_segments`, real store rows) in a per-test
    // temp directory so `save_manual_translation` / `set_segment_flag` /
    // `retry_segment_with_guidance` exercise the same code paths production
    // traffic does. Every test gets its own `tempfile::TempDir` and its own
    // `AppState::store_path` (via `test_state_with_store`), so nothing here
    // touches the process's current directory and tests are parallel-safe.
    // -----------------------------------------------------------------------

    use bookforge_core::segment::BlockTranslation;
    use bookforge_store::{CreateJob, SaveTranslation};
    use std::io::Write as _;

    const FIXTURE_CONTAINER_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;

    const FIXTURE_OPF: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="uid">serve-fixture</dc:identifier>
    <dc:title>Serve Fixture</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="ch1" href="chapter1.xhtml" media-type="application/xhtml+xml"/>
    <item id="ch2" href="chapter2.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="ch1"/>
    <itemref idref="ch2"/>
  </spine>
</package>"#;

    const FIXTURE_CHAPTER_ONE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<head><title>Fixture Chapter One</title></head>
<body>
<p>The lantern flickered in the old library.</p>
</body>
</html>"#;

    const FIXTURE_CHAPTER_TWO: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<head><title>Fixture Chapter Two</title></head>
<body>
<p>Rain tapped steadily against the windowpane.</p>
</body>
</html>"#;

    fn build_fixture_epub(path: &std::path::Path) {
        use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

        let file = std::fs::File::create(path).expect("fixture EPUB should be creatable");
        let mut zip = ZipWriter::new(file);
        let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

        zip.start_file("mimetype", stored).unwrap();
        zip.write_all(b"application/epub+zip").unwrap();
        zip.start_file("META-INF/container.xml", deflated).unwrap();
        zip.write_all(FIXTURE_CONTAINER_XML.as_bytes()).unwrap();
        zip.start_file("content.opf", deflated).unwrap();
        zip.write_all(FIXTURE_OPF.as_bytes()).unwrap();
        zip.start_file("chapter1.xhtml", deflated).unwrap();
        zip.write_all(FIXTURE_CHAPTER_ONE.as_bytes()).unwrap();
        zip.start_file("chapter2.xhtml", deflated).unwrap();
        zip.write_all(FIXTURE_CHAPTER_TWO.as_bytes()).unwrap();
        zip.finish().unwrap();
    }

    /// A completed two-segment job (one segment per chapter, one block per
    /// segment) backed by an isolated temp-dir store and a real rebuildable
    /// output path — everything the mutation endpoints under test touch.
    struct MutationFixture {
        // Held only to keep the temp directory alive for the fixture's
        // lifetime; never read directly.
        _temp: tempfile::TempDir,
        store_path: PathBuf,
        output_path: PathBuf,
        job_id: String,
        segment_a: String,
        segment_b: String,
        csrf: String,
    }

    fn build_mutation_fixture() -> MutationFixture {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let input_path = temp.path().join("input.epub");
        build_fixture_epub(&input_path);
        let output_path = temp.path().join("output.epub");
        let store_path = temp.path().join("jobs.sqlite");

        let store = JobStore::open(&store_path).expect("store should open");
        let job = store
            .create_job(CreateJob {
                input: &input_path,
                output: &output_path,
                source_lang: Some("English"),
                target_lang: "Italian",
                provider: "mock",
                model: "mock-identity",
                base_url: None,
                api_key_env: None,
                book_id: None,
                series_id: None,
            })
            .expect("job should be created");

        let book = bookforge_epub::read_epub(&input_path).expect("fixture EPUB should parse");
        let settings = bookforge_core::TranslationProfile::Balanced.resolve();
        let segments = bookforge_core::segment::build_segments(&book, &settings.segmentation)
            .expect("segments should build");
        // `read_epub`/`build_segments` also synthesize a segment for the OPF
        // `dc:title` metadata (its own section, ahead of the spine chapters),
        // so a two-chapter book yields three segments total. Only the two
        // chapter segments are used as `segment_a`/`segment_b` below; the
        // metadata segment is still inserted and translated like any other
        // so the fixture matches what a real job actually persists.
        let chapter_segments = segments
            .iter()
            .filter(|segment| segment.section_id.0 != "sec_metadata_opf")
            .collect::<Vec<_>>();
        assert_eq!(
            chapter_segments.len(),
            2,
            "fixture EPUB (one paragraph per chapter) should yield exactly two chapter segments"
        );

        store
            .insert_segments(&job.id, &segments, "v1", "mock", "mock-identity", "test_ns")
            .expect("segments should insert");

        for segment in &segments {
            let blocks = segment
                .source
                .blocks
                .iter()
                .map(|block| BlockTranslation {
                    block_id: block.block_id.clone(),
                    text: format!("[IT] {}", block.text),
                })
                .collect::<Vec<_>>();
            let translated_text = blocks
                .iter()
                .map(|block| block.text.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            store
                .save_translation(SaveTranslation {
                    job_id: &job.id,
                    segment_id: &segment.id.0,
                    translated_text: &translated_text,
                    blocks: &blocks,
                    input_tokens: Some(10),
                    input_cached_tokens: Some(0),
                    output_tokens: Some(12),
                    tokens_estimated: false,
                    provider: "mock",
                    model: "mock-identity",
                    prompt_version: "v1",
                })
                .expect("translation should save");
        }
        store
            .mark_job_complete(&job.id)
            .expect("job should complete");

        let snapshot = bookforge_core::RunConfigSnapshot {
            input_path: input_path.clone(),
            input_snapshot_path: Some(input_path.clone()),
            input_sha256: Some("test-sha".to_string()),
            output_path: output_path.clone(),
            events_path: None,
            report_json_path: None,
            report_markdown_path: None,
            source_language: Some("English".to_string()),
            target_language: "Italian".to_string(),
            provider: "mock".to_string(),
            model: "mock-identity".to_string(),
            base_url: None,
            api_key_env: None,
            profile: settings.profile,
            provider_preset: None,
            prompt_version: "v1".to_string(),
            cache_namespace: "test_ns".to_string(),
            book_id: None,
            series_id: None,
            glossary_budget_tokens: 800,
            glossary_format: bookforge_core::GlossaryFormat::Json,
            prompt_extra: None,
            glossary_fingerprint: String::new(),
            glossary_terms: Vec::new(),
            context_window: 0,
            context_budget_tokens: 1200,
            context_scope: bookforge_core::config::ContextScope::Chapter,
            style_fingerprint: String::new(),
            style_rendered_block: String::new(),
            entities_fingerprint: String::new(),
            entities_rendered_block: String::new(),
            bilingual_mode: bookforge_core::BilingualMode::Replace,
            bilingual_separator: " / ".to_string(),
            bilingual_style: bookforge_core::BilingualStyle::Minimal,
            bilingual_css: None,
            fallback: None,
            finalize: bookforge_core::run_snapshot::FinalizeCheckpointSnapshot::default(),
            qa_mode: "off".to_string(),
            validate_output: false,
            settings: bookforge_core::ResolvedRunSettingsSnapshot::from_settings(&settings),
        };
        store
            .update_job_config_snapshot(&job.id, &snapshot)
            .expect("snapshot should persist");

        MutationFixture {
            store_path,
            output_path,
            job_id: job.id,
            segment_a: chapter_segments[0].id.0.clone(),
            segment_b: chapter_segments[1].id.0.clone(),
            csrf: "fixture-csrf-token".to_string(),
            _temp: temp,
        }
    }

    /// Sends `body` to `uri` with the given CSRF header value (or none), and
    /// returns the response.
    async fn post_json(
        router: &Router,
        uri: &str,
        csrf: Option<&str>,
        body: serde_json::Value,
    ) -> Response {
        use axum::{body::Body, http::Request};
        use tower::ServiceExt;

        let mut builder = Request::builder()
            .method("POST")
            .uri(uri)
            .header("host", TEST_HOST)
            .header("content-type", "application/json");
        if let Some(token) = csrf {
            builder = builder.header(CSRF_HEADER, token);
        }
        router
            .clone()
            .oneshot(
                builder
                    .body(Body::from(body.to_string()))
                    .expect("request should build"),
            )
            .await
            .expect("route should respond")
    }

    async fn get_route(router: &Router, uri: &str) -> Response {
        use axum::{body::Body, http::Request};
        use tower::ServiceExt;

        router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header("host", TEST_HOST)
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("route should respond")
    }

    async fn response_json(response: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body should read");
        serde_json::from_slice(&bytes).expect("response should be JSON")
    }

    fn make_stopped_fixture_resumable(fixture: &MutationFixture) {
        let store = JobStore::open(&fixture.store_path).expect("store should reopen");
        store
            .request_segment_retry(&fixture.job_id, &fixture.segment_b, None)
            .expect("completed segment should become retry-pending");
        store
            .mark_job_stopped(&fixture.job_id)
            .expect("job should become stopped");
    }

    fn clean_runtime_files(job_id: &str) {
        let _ = std::fs::remove_dir_all(bookforge_core::run_dir_for_job(job_id));
    }

    #[tokio::test]
    async fn dashboard_reconfigure_is_typed_revisioned_and_csrf_protected() {
        let fixture = build_mutation_fixture();
        make_stopped_fixture_resumable(&fixture);
        clean_runtime_files(&fixture.job_id);
        let router = dashboard_router(test_state_with_store(
            &fixture.csrf,
            fixture.store_path.clone(),
        ));
        let uri = format!("/api/jobs/{}/reconfigure", fixture.job_id);

        let initial = get_route(&router, &uri).await;
        assert_eq!(initial.status(), StatusCode::OK);
        let initial = response_json(initial).await;
        assert_eq!(initial["revision"], 0);
        assert_eq!(initial["applied_revision"], 0);
        assert_eq!(initial["application_state"], "resume_required");
        assert_eq!(initial["lease"]["state"], "missing");
        assert_eq!(initial["identity"]["provider"], "mock");
        assert_eq!(initial["identity"]["model"], "mock-identity");
        assert_eq!(initial["editable"], true);

        let body = json!({ "concurrency": 2 });
        let missing = post_json(&router, &uri, None, body.clone()).await;
        assert_eq!(missing.status(), StatusCode::FORBIDDEN);
        let wrong = post_json(&router, &uri, Some("wrong-token"), body).await;
        assert_eq!(wrong.status(), StatusCode::FORBIDDEN);
        assert!(
            !crate::commands::reconfigure::overrides_path_for_job(&fixture.job_id).exists(),
            "rejected mutations must not create a sidecar"
        );

        let unknown = post_json(
            &router,
            &uri,
            Some(&fixture.csrf),
            json!({ "model": "immutable-model" }),
        )
        .await;
        assert!(unknown.status().is_client_error());
        assert!(
            !crate::commands::reconfigure::overrides_path_for_job(&fixture.job_id).exists(),
            "unknown or immutable fields must not create a sidecar"
        );

        let first = post_json(
            &router,
            &uri,
            Some(&fixture.csrf),
            json!({
                "batch_max_output_tokens": 12000,
                "batch_max_items": 2,
                "batch_target_tokens": 3000,
                "concurrency": 2,
                "qa": "all",
                "double_check": "Formatting",
                "validate_output": true,
                "provider_max_attempts": 5,
                "adaptive_concurrency": false,
                "adaptive_batch_sizing": false
            }),
        )
        .await;
        assert_eq!(first.status(), StatusCode::OK);
        let first = response_json(first).await;
        assert_eq!(first["revision"], 1);
        assert_eq!(first["effective"]["concurrency"], 2);
        assert_eq!(first["effective"]["qa"], "all");
        assert_eq!(first["effective"]["double_check"], "Formatting");
        assert_eq!(first["effective"]["validate_output"], true);
        assert_eq!(first["application_state"], "resume_required");
        let fields = first["changed_fields"]
            .as_array()
            .expect("changed fields should be an array");
        assert!(fields.iter().any(|field| field == "concurrency"));
        assert!(fields.iter().any(|field| field == "qa"));
        let boundaries = first["next_boundary"]
            .as_array()
            .expect("boundaries should be an array");
        for boundary in ["next_request", "next_batch", "next_stage"] {
            assert!(boundaries.iter().any(|value| value == boundary));
        }

        let second = post_json(
            &router,
            &uri,
            Some(&fixture.csrf),
            json!({ "concurrency": 3 }),
        )
        .await;
        assert_eq!(second.status(), StatusCode::OK);
        let second = response_json(second).await;
        assert_eq!(second["revision"], 2);
        assert_eq!(second["effective"]["concurrency"], 3);
        assert_eq!(second["effective"]["batch_max_items"], 2);

        let replayed = response_json(get_route(&router, &uri).await).await;
        assert_eq!(replayed["revision"], 2);
        assert_eq!(replayed["effective"]["concurrency"], 3);

        clean_runtime_files(&fixture.job_id);
    }

    #[tokio::test]
    async fn dashboard_controls_require_a_fresh_lease_and_signal_one_when_present() {
        let fixture = build_mutation_fixture();
        make_stopped_fixture_resumable(&fixture);
        clean_runtime_files(&fixture.job_id);
        let router = dashboard_router(test_state_with_store(
            &fixture.csrf,
            fixture.store_path.clone(),
        ));

        for command in ["pause", "stop"] {
            let response = post_json(
                &router,
                &format!("/api/jobs/{}/{}", fixture.job_id, command),
                Some(&fixture.csrf),
                json!({}),
            )
            .await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let payload = response_json(response).await;
            assert!(
                payload["error"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("no live worker")
            );
        }

        let lease = crate::control::RuntimeLease {
            schema_version: 1,
            instance_id: "dashboard-test-worker".to_string(),
            pid: std::process::id(),
            process_started_at_ms: bookforge_core::now_ms(),
            heartbeat_at_ms: bookforge_core::now_ms(),
            last_loaded_revision: 4,
            last_applied_revision: 4,
        };
        let runtime_path = crate::control::runtime_path_for_job(&fixture.job_id);
        std::fs::create_dir_all(runtime_path.parent().expect("runtime parent"))
            .expect("runtime directory should exist");
        std::fs::write(
            &runtime_path,
            serde_json::to_vec_pretty(&lease).expect("lease should serialize"),
        )
        .expect("lease should write");

        let view = response_json(
            get_route(
                &router,
                &format!("/api/jobs/{}/reconfigure", fixture.job_id),
            )
            .await,
        )
        .await;
        assert_eq!(view["lease"]["state"], "fresh");
        assert_eq!(view["live"], true);
        assert_eq!(view["applied_revision"], 4);

        let resume = post_json(
            &router,
            &format!("/api/jobs/{}/resume", fixture.job_id),
            Some(&fixture.csrf),
            json!({}),
        )
        .await;
        assert_eq!(resume.status(), StatusCode::OK);
        let resume = response_json(resume).await;
        assert_eq!(resume["mode"], "signaled");
        assert_eq!(
            bookforge_core::read_control_file(&bookforge_core::control_path_for_job(
                &fixture.job_id
            ))
            .expect("control file should read"),
            bookforge_core::ControlCommand::Resume
        );

        clean_runtime_files(&fixture.job_id);
    }

    #[tokio::test]
    async fn dashboard_missing_worker_resume_launches_exactly_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let fixture = build_mutation_fixture();
        make_stopped_fixture_resumable(&fixture);
        clean_runtime_files(&fixture.job_id);
        let launches = Arc::new(AtomicUsize::new(0));
        let mut state = test_state_with_store(&fixture.csrf, fixture.store_path.clone());
        state.resume_launches = Some(launches.clone());
        let router = dashboard_router(state);
        let uri = format!("/api/jobs/{}/resume", fixture.job_id);

        let (first, second) = tokio::join!(
            post_json(&router, &uri, Some(&fixture.csrf), json!({})),
            post_json(&router, &uri, Some(&fixture.csrf), json!({}))
        );
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(second.status(), StatusCode::OK);
        let first = response_json(first).await;
        let second = response_json(second).await;
        let modes = [
            first["mode"].as_str().unwrap_or_default(),
            second["mode"].as_str().unwrap_or_default(),
        ];
        assert!(modes.contains(&"spawned"));
        assert!(modes.contains(&"launching"));
        assert_eq!(
            launches.load(Ordering::SeqCst),
            1,
            "the atomic launch claim must deduplicate concurrent Resume clicks"
        );

        clean_runtime_files(&fixture.job_id);
    }

    #[tokio::test]
    async fn dashboard_resume_recognizes_finalize_only_work() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let fixture = build_mutation_fixture();
        let store = JobStore::open(&fixture.store_path).expect("store should reopen");
        assert!(
            store
                .resumable_segment_ids(&fixture.job_id)
                .expect("segment lookup should succeed")
                .is_empty(),
            "the fixture should have no translation work left"
        );
        store
            .mark_job_stopped(&fixture.job_id)
            .expect("job should become stopped during finalization");
        clean_runtime_files(&fixture.job_id);

        let launches = Arc::new(AtomicUsize::new(0));
        let mut state = test_state_with_store(&fixture.csrf, fixture.store_path.clone());
        state.resume_launches = Some(launches.clone());
        let router = dashboard_router(state);

        let view = response_json(
            get_route(
                &router,
                &format!("/api/jobs/{}/reconfigure", fixture.job_id),
            )
            .await,
        )
        .await;
        assert_eq!(view["resumable_work"], true);
        assert_eq!(view["editable"], true);

        let response = post_json(
            &router,
            &format!("/api/jobs/{}/resume", fixture.job_id),
            Some(&fixture.csrf),
            json!({}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let response = response_json(response).await;
        assert_eq!(response["mode"], "spawned");
        assert_eq!(response["forced"], false);
        assert_eq!(launches.load(Ordering::SeqCst), 1);

        clean_runtime_files(&fixture.job_id);
    }

    #[tokio::test]
    async fn dashboard_resume_forces_relaunch_of_a_dead_paused_worker() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let fixture = build_mutation_fixture();
        let store = JobStore::open(&fixture.store_path).expect("store should reopen");
        store
            .mark_job_paused(&fixture.job_id)
            .expect("job should become paused");
        clean_runtime_files(&fixture.job_id);

        let launches = Arc::new(AtomicUsize::new(0));
        let mut state = test_state_with_store(&fixture.csrf, fixture.store_path.clone());
        state.resume_launches = Some(launches.clone());
        let router = dashboard_router(state);
        let response = post_json(
            &router,
            &format!("/api/jobs/{}/resume", fixture.job_id),
            Some(&fixture.csrf),
            json!({}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let response = response_json(response).await;
        assert_eq!(response["mode"], "spawned");
        assert_eq!(response["forced"], true);
        assert_eq!(launches.load(Ordering::SeqCst), 1);

        clean_runtime_files(&fixture.job_id);
    }

    #[tokio::test]
    async fn dashboard_resume_rejects_a_completed_job_without_work() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let fixture = build_mutation_fixture();
        clean_runtime_files(&fixture.job_id);
        let launches = Arc::new(AtomicUsize::new(0));
        let mut state = test_state_with_store(&fixture.csrf, fixture.store_path.clone());
        state.resume_launches = Some(launches.clone());
        let router = dashboard_router(state);

        let view = response_json(
            get_route(
                &router,
                &format!("/api/jobs/{}/reconfigure", fixture.job_id),
            )
            .await,
        )
        .await;
        assert_eq!(view["resumable_work"], false);
        assert_eq!(view["editable"], false);

        let response = post_json(
            &router,
            &format!("/api/jobs/{}/resume", fixture.job_id),
            Some(&fixture.csrf),
            json!({}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(
            response_json(response).await["error"]
                .as_str()
                .unwrap_or_default()
                .contains("no resumable work")
        );
        assert_eq!(launches.load(Ordering::SeqCst), 0);

        clean_runtime_files(&fixture.job_id);
    }

    #[tokio::test]
    async fn save_manual_translation_rejects_missing_or_wrong_csrf_without_mutating_store() {
        let fixture = build_mutation_fixture();
        let router = dashboard_router(test_state_with_store(
            &fixture.csrf,
            fixture.store_path.clone(),
        ));
        let uri = format!(
            "/api/jobs/{}/segments/{}/translation",
            fixture.job_id, fixture.segment_a
        );
        let body = json!({ "blocks": [{ "block_id": "whatever", "text": "corrupted" }] });

        let missing = post_json(&router, &uri, None, body.clone()).await;
        assert_eq!(missing.status(), StatusCode::FORBIDDEN);

        let wrong = post_json(&router, &uri, Some("wrong-token"), body).await;
        assert_eq!(wrong.status(), StatusCode::FORBIDDEN);

        let store = JobStore::open(&fixture.store_path).expect("store should reopen");
        assert!(
            !store
                .translation_is_human_corrected(&fixture.job_id, &fixture.segment_a)
                .expect("lookup should succeed"),
            "a rejected request must not human-correct the segment"
        );
    }

    #[tokio::test]
    async fn set_segment_flag_rejects_missing_or_wrong_csrf_without_mutating_store() {
        let fixture = build_mutation_fixture();
        let router = dashboard_router(test_state_with_store(
            &fixture.csrf,
            fixture.store_path.clone(),
        ));
        let uri = format!(
            "/api/jobs/{}/segments/{}/flag",
            fixture.job_id, fixture.segment_b
        );
        let body = json!({ "flagged": true });

        let missing = post_json(&router, &uri, None, body.clone()).await;
        assert_eq!(missing.status(), StatusCode::FORBIDDEN);

        let wrong = post_json(&router, &uri, Some("wrong-token"), body).await;
        assert_eq!(wrong.status(), StatusCode::FORBIDDEN);

        let store = JobStore::open(&fixture.store_path).expect("store should reopen");
        let flagged = store
            .dashboard_flagged_segment_ids(&fixture.job_id)
            .expect("flags should load");
        assert!(
            !flagged.contains(&fixture.segment_b),
            "a rejected request must not persist a flag"
        );
    }

    #[tokio::test]
    async fn retry_segment_rejects_missing_or_wrong_csrf_without_mutating_store() {
        let fixture = build_mutation_fixture();
        let router = dashboard_router(test_state_with_store(
            &fixture.csrf,
            fixture.store_path.clone(),
        ));
        let uri = format!(
            "/api/jobs/{}/segments/{}/retry",
            fixture.job_id, fixture.segment_b
        );
        let body = json!({ "guidance": "please redo" });

        let missing = post_json(&router, &uri, None, body.clone()).await;
        assert_eq!(missing.status(), StatusCode::FORBIDDEN);

        let wrong = post_json(&router, &uri, Some("wrong-token"), body).await;
        assert_eq!(wrong.status(), StatusCode::FORBIDDEN);

        let store = JobStore::open(&fixture.store_path).expect("store should reopen");
        let guidance = store
            .load_retry_guidance(&fixture.job_id)
            .expect("guidance should load");
        assert!(
            !guidance.contains_key(&fixture.segment_b),
            "a rejected request must not persist retry guidance"
        );
        let records = store
            .segment_records(&fixture.job_id)
            .expect("records should load");
        let segment_b = records
            .iter()
            .find(|record| record.id == fixture.segment_b)
            .expect("segment_b should exist");
        assert_eq!(
            segment_b.status, "succeeded",
            "a rejected retry must not move the segment to retry_pending"
        );
    }

    #[tokio::test]
    async fn dashboard_review_and_mutation_endpoints_end_to_end() {
        use axum::{body::Body, http::Request};
        use tower::ServiceExt;

        let fixture = build_mutation_fixture();
        let router = dashboard_router(test_state_with_store(
            &fixture.csrf,
            fixture.store_path.clone(),
        ));

        // 1. Review data: per-block source/target text is present for both segments.
        let review_uri = format!("/api/jobs/{}/review", fixture.job_id);
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(&review_uri)
                    .header("host", TEST_HOST)
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("route should respond");
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body should read");
        let review: serde_json::Value =
            serde_json::from_slice(&bytes).expect("review should be json");
        let segments = review["segments"].as_array().expect("segments array");
        // Two chapter segments plus the synthesized OPF `dc:title` metadata
        // segment (see `build_mutation_fixture`).
        assert_eq!(segments.len(), 3);
        for segment in segments {
            let blocks = segment["blocks"].as_array().expect("blocks array");
            assert!(!blocks.is_empty(), "each segment should have blocks");
            for block in blocks {
                assert!(
                    !block["target_text"].as_str().unwrap_or_default().is_empty(),
                    "each block should carry non-empty target text"
                );
            }
        }

        // 2. Save a corrected translation for segment_a.
        let segment_a_json = segments
            .iter()
            .find(|segment| segment["segment_id"] == fixture.segment_a)
            .expect("segment_a should appear in the review");
        let block_ids = segment_a_json["blocks"]
            .as_array()
            .expect("blocks array")
            .iter()
            .map(|block| {
                block["block_id"]
                    .as_str()
                    .expect("block_id should be a string")
                    .to_string()
            })
            .collect::<Vec<_>>();
        let correction_body = json!({
            "blocks": block_ids
                .iter()
                .map(|id| json!({ "block_id": id, "text": "Corrected by reviewer." }))
                .collect::<Vec<_>>(),
        });
        let translation_uri = format!(
            "/api/jobs/{}/segments/{}/translation",
            fixture.job_id, fixture.segment_a
        );
        let response = post_json(
            &router,
            &translation_uri,
            Some(&fixture.csrf),
            correction_body,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let store = JobStore::open(&fixture.store_path).expect("store should reopen");
        assert!(
            store
                .translation_is_human_corrected(&fixture.job_id, &fixture.segment_a)
                .expect("lookup should succeed"),
            "segment_a should be marked human_corrected"
        );
        assert!(
            fixture.output_path.exists(),
            "the rebuilt output EPUB should exist after the correction"
        );

        // 3. Flag, then clear, segment_b.
        let flag_uri = format!(
            "/api/jobs/{}/segments/{}/flag",
            fixture.job_id, fixture.segment_b
        );
        for flagged in [true, false] {
            let response = post_json(
                &router,
                &flag_uri,
                Some(&fixture.csrf),
                json!({ "flagged": flagged }),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            let flagged_ids = store
                .dashboard_flagged_segment_ids(&fixture.job_id)
                .expect("flags should load");
            assert_eq!(flagged_ids.contains(&fixture.segment_b), flagged);
        }

        // 4. Request a retry with guidance for segment_b.
        let retry_uri = format!(
            "/api/jobs/{}/segments/{}/retry",
            fixture.job_id, fixture.segment_b
        );
        let response = post_json(
            &router,
            &retry_uri,
            Some(&fixture.csrf),
            json!({ "guidance": "Please redo more literally." }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let guidance = store
            .load_retry_guidance(&fixture.job_id)
            .expect("guidance should load");
        assert_eq!(
            guidance.get(&fixture.segment_b).map(String::as_str),
            Some("Please redo more literally.")
        );
        let records = store
            .segment_records(&fixture.job_id)
            .expect("records should load");
        let segment_b_record = records
            .iter()
            .find(|record| record.id == fixture.segment_b)
            .expect("segment_b should exist");
        assert_eq!(segment_b_record.status, "retry_pending");

        // 5. A retry request against the now-frozen (human-corrected) segment_a
        //    is rejected, and does not disturb its correction.
        let retry_a_uri = format!(
            "/api/jobs/{}/segments/{}/retry",
            fixture.job_id, fixture.segment_a
        );
        let response = post_json(
            &router,
            &retry_a_uri,
            Some(&fixture.csrf),
            json!({ "guidance": "try again" }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body should read");
        let payload: serde_json::Value =
            serde_json::from_slice(&bytes).expect("error body should be json");
        assert!(
            payload["error"]
                .as_str()
                .unwrap_or_default()
                .contains("frozen human correction"),
            "rejection should explain the segment is frozen: {payload}"
        );
        assert!(
            store
                .translation_is_human_corrected(&fixture.job_id, &fixture.segment_a)
                .expect("lookup should succeed"),
            "the rejected retry must not un-freeze the human correction"
        );
    }
}
