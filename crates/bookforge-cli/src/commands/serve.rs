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
    process::Command,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Multipart, Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode},
    response::{
        Html, IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{delete, get, post},
};
use bookforge_core::{
    GlossaryCategory, GlossaryScopeKind, GlossaryStatus, GlossaryTerm, RunState, now_ms,
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
    /// Provider → API key, supplied via the dashboard. Held only in memory for
    /// the lifetime of the server: never written to disk, never logged, and
    /// only injected into spawned runs through the child's environment.
    keys: Arc<Mutex<HashMap<String, String>>>,
}

pub async fn run(args: ServeArgs) -> Result<()> {
    let addr: SocketAddr = args
        .bind
        .parse()
        .with_context(|| format!("invalid --bind address '{}'", args.bind))?;
    require_loopback_bind(addr)?;

    let state = AppState {
        refresh: Duration::from_millis(args.refresh_ms.clamp(50, 5_000)),
        csrf_token: generate_csrf_token()?,
        keys: Arc::new(Mutex::new(HashMap::new())),
    };

    let app = dashboard_router(state);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;
    let local = listener.local_addr().unwrap_or(addr);
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
    Router::new()
        .route("/", get(index))
        .route("/api/jobs", get(list_jobs))
        .route("/api/jobs/{id}", get(job_detail))
        .route("/api/jobs/{id}/events", get(job_events))
        .route("/api/jobs/{id}/review", get(job_review))
        .route("/api/jobs/{id}/validate", post(job_validate))
        .route("/api/jobs/{id}/retry", post(retry_job))
        .route("/api/options", get(dashboard_options))
        .route("/api/providers", get(provider_status))
        .route("/api/estimate", post(estimate_translate))
        .route("/api/translate", post(launch_translate))
        .route("/api/glossary", get(list_glossary).post(add_glossary))
        .route("/api/glossary/{id}", delete(remove_glossary))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
        .with_state(state)
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

async fn list_jobs() -> Result<Json<Vec<JobListItem>>, AppError> {
    let items = tokio::task::spawn_blocking(|| -> Result<Vec<JobListItem>> {
        let store = JobStore::open_default()?;
        Ok(store
            .list_job_summaries()?
            .into_iter()
            .map(|(job, summary)| JobListItem::new(&job, &summary))
            .collect())
    })
    .await??;
    Ok(Json(items))
}

async fn job_detail(AxumPath(id): AxumPath<String>) -> Result<Response, AppError> {
    let lookup = id.clone();
    let detail = tokio::task::spawn_blocking(move || -> Result<Option<JobDetail>> {
        let store = JobStore::open_default()?;
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

async fn job_events(
    AxumPath(id): AxumPath<String>,
    State(state): State<AppState>,
) -> Sse<impl futures_core::Stream<Item = Result<Event, Infallible>>> {
    let refresh = state.refresh;
    let path = resolve_events_path(id).await;

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
async fn job_review(AxumPath(id): AxumPath<String>) -> Result<Response, AppError> {
    let outcome = tokio::task::spawn_blocking(move || {
        let store = JobStore::open_default()?;
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

    let outcome = tokio::task::spawn_blocking(
        move || -> Result<Option<super::validate::ValidationReport>> {
            let store = JobStore::open_default()?;
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
    let retried = tokio::task::spawn_blocking(move || -> Result<usize> {
        let store = JobStore::open_default()?;
        Ok(store.retry_segments(&id, RetryScope::All)?)
    })
    .await??;
    Ok(Json(json!({ "retried": retried })).into_response())
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
    if provider == "openai-compatible" {
        if field_value(&fields, "base_url").is_none() {
            return Ok(bad_request("base URL is required for openai-compatible"));
        }
        if field_value(&fields, "model").is_none() {
            return Ok(bad_request("model is required for openai-compatible"));
        }
    }

    // Resolve the API key: a freshly-supplied one is remembered for the session;
    // otherwise reuse one already remembered for this provider. A blank key falls
    // through to the run's normal environment-variable resolution.
    let supplied_key = (provider != "mock")
        .then(|| field_value(&fields, "api_key"))
        .flatten();
    let key = if let Some(supplied) = supplied_key {
        state
            .keys
            .lock()
            .expect("keys mutex poisoned")
            .insert(provider.clone(), supplied.clone());
        Some(supplied)
    } else {
        state
            .keys
            .lock()
            .expect("keys mutex poisoned")
            .get(&provider)
            .cloned()
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
    if provider == "openai-compatible"
        && let Some(base_url) = field_value(&fields, "base_url")
    {
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

    // Detached: the run outlives this request. Errors surface in the serve
    // console (inherited stdio) and as failure status on the job itself.
    command
        .spawn()
        .context("failed to spawn translation process")?;

    Ok(Json(json!({
        "ok": true,
        "input_path": input_path.display().to_string(),
        "provider": provider,
    }))
    .into_response())
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
async fn provider_status(State(state): State<AppState>) -> Json<serde_json::Value> {
    let remembered = state.keys.lock().expect("keys mutex poisoned");
    let mut status = serde_json::Map::new();
    for (provider, env) in PROVIDER_KEY_ENVS {
        let configured = remembered.contains_key(*provider)
            || std::env::var(env).map(|v| !v.is_empty()).unwrap_or(false);
        status.insert((*provider).to_string(), json!(configured));
    }
    Json(serde_json::Value::Object(status))
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
) -> Result<Json<Vec<serde_json::Value>>, AppError> {
    let items = tokio::task::spawn_blocking(move || -> Result<Vec<serde_json::Value>> {
        let store = JobStore::open_default()?;
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

    let id = tokio::task::spawn_blocking(move || -> Result<i64> {
        let store = JobStore::open_default()?;
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
    let removed = tokio::task::spawn_blocking(move || -> Result<usize> {
        let store = JobStore::open_default()?;
        Ok(store.remove_glossary_term(id)?)
    })
    .await??;
    Ok(Json(json!({ "removed": removed })).into_response())
}

fn supported_provider(provider: &str) -> bool {
    provider == "mock" || provider_key_env(provider).is_some()
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
        Some(token) if token == state.csrf_token => None,
        _ => Some(forbidden("missing or invalid dashboard token")),
    }
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
async fn resolve_events_path(id: String) -> PathBuf {
    let fallback = PathBuf::from(format!(".bookforge/runs/{id}/events.jsonl"));
    let lookup = id.clone();
    tokio::task::spawn_blocking(move || {
        let job = JobStore::open_default()
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

const DASHBOARD_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>BookForge</title>
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link href="https://fonts.googleapis.com/css2?family=Spectral:ital,wght@0,400;0,500;0,600;0,700;1,400&family=IBM+Plex+Sans:wght@400;500;600;700&family=IBM+Plex+Mono:wght@400;500;600&display=swap" rel="stylesheet">
<style>
:root{
  --bg:#f4f1ea; --panel:#fcfbf8; --card:#ffffff; --line:#e6dfd0; --soft:#f6f2ea;
  --ink:#2a2521; --muted:#7a7066; --faint:#a89e90; --accent:#cf8a2d; --accentink:#ffffff;
  --chip:#efe9dd; --good:#5b8c5a; --goodbg:#eef3ec; --warn:#c98a2d; --danger:#c2502f;
  --accentsoft:#faf3e6; --accentline:#ecd9b3;
  --sans:'IBM Plex Sans',system-ui,-apple-system,'Segoe UI',Roboto,sans-serif;
  --serif:'Spectral',Georgia,'Times New Roman',serif;
  --mono:'IBM Plex Mono',ui-monospace,SFMono-Regular,Menlo,monospace;
}
:root[data-theme="dark"]{
  --bg:#1b1814; --panel:#211d18; --card:#2a251e; --line:#3a332a; --soft:#241f19;
  --ink:#ece5da; --muted:#b3a99b; --faint:#7d7468; --accent:#d97a52; --accentink:#1a1713;
  --chip:#2c2720; --good:#6aa765; --goodbg:#23291f; --warn:#d6a24a; --danger:#e07a55;
  --accentsoft:#2a221c; --accentline:#473829;
}
*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--ink);font:14px/1.5 var(--sans)}
::selection{background:rgba(207,138,45,.25)}
.scr::-webkit-scrollbar{width:9px;height:9px}
.scr::-webkit-scrollbar-thumb{background:var(--line);border-radius:5px}
@keyframes bf-fade{from{opacity:0;transform:translateY(8px)}to{opacity:1;transform:none}}
@keyframes bf-stripe{from{background-position:0 0}to{background-position:40px 0}}
input,textarea,select,button{font-family:inherit}
a{color:var(--accent)}

.appbar{position:sticky;top:0;z-index:30;display:flex;align-items:center;gap:16px;padding:0 26px;height:60px;background:var(--panel);border-bottom:1px solid var(--line)}
.brand{display:flex;align-items:baseline;gap:9px;cursor:pointer}
.brand b{font:600 21px var(--serif);letter-spacing:-.01em;color:var(--ink)}
.brand small{font:400 11px var(--mono);color:var(--faint)}
.nav{display:flex;gap:3px;margin-left:12px}
.tab{padding:8px 12px;border-radius:8px;cursor:pointer;font:500 13px var(--sans);color:var(--muted)}
.tab:hover{color:var(--ink)}
.tab.active{font-weight:600;color:var(--ink);background:var(--chip)}
.spacer{margin-left:auto}
.appbar .right{display:flex;align-items:center;gap:13px}
.btn{border:none;border-radius:9px;cursor:pointer;font:600 13px var(--sans)}
.btn-primary{background:var(--accent);color:var(--accentink);padding:8px 15px}
.btn-ghost{background:transparent;border:1px solid var(--line);color:var(--ink);padding:10px 18px;border-radius:10px}
.btn-danger{background:transparent;border:1px solid var(--line);color:var(--danger);padding:12px 18px;border-radius:10px}
.themetoggle{display:flex;background:var(--chip);border-radius:20px;padding:3px;gap:2px;cursor:pointer;user-select:none}
.themetoggle span{font:600 11px var(--sans);padding:5px 11px;border-radius:16px;color:var(--faint)}
.themetoggle span.on{background:var(--panel);color:var(--ink);box-shadow:0 1px 2px rgba(0,0,0,.12)}

main{min-height:calc(100vh - 60px)}
.wrap{max-width:1000px;margin:0 auto;padding:38px 26px 80px;animation:bf-fade .4s ease both}
.pagehead{display:flex;align-items:flex-end;justify-content:space-between;margin-bottom:24px;gap:16px}
.pagehead h1{font:600 28px/1.1 var(--serif);margin:0;color:var(--ink)}
.pagehead p{font:400 13.5px var(--sans);color:var(--muted);margin:5px 0 0}
.empty{color:var(--muted);text-align:center;margin-top:60px;font-size:14px}

/* library */
.book-grid{display:grid;grid-template-columns:1fr 1fr;gap:14px}
.book-card{display:flex;gap:15px;padding:16px;background:var(--card);border:1px solid var(--line);border-radius:14px;cursor:pointer}
.book-card:hover{border-color:var(--accentline)}
.cover{width:56px;height:80px;flex:none;border-radius:6px;display:flex;align-items:center;justify-content:center;font:600 24px var(--serif);color:var(--accentink);background:linear-gradient(150deg,var(--accent),var(--danger));box-shadow:0 6px 14px -6px rgba(0,0,0,.4)}
.book-main{flex:1;min-width:0}
.book-title{font:600 16px/1.2 var(--serif);color:var(--ink);white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.book-sub{font:italic 400 12.5px var(--serif);color:var(--muted);margin:2px 0 9px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.book-meta{display:flex;align-items:center;gap:8px;margin-bottom:11px}
.book-meta .mono{font:400 11.5px var(--mono);color:var(--faint)}
.bar-track{height:5px;border-radius:3px;background:var(--chip);overflow:hidden}
.bar-fill{height:100%;border-radius:3px;background:var(--accent)}
.book-action{font:500 12.5px var(--sans);color:var(--accent);white-space:nowrap;align-self:flex-start}
.add-card{display:flex;flex-direction:column;align-items:center;justify-content:center;padding:22px;border:1.5px dashed var(--line);border-radius:14px;cursor:pointer;background:var(--soft);min-height:140px;text-align:center}
.add-card .plus{font-size:26px;line-height:1;color:var(--accent)}
.add-card b{font:600 14px var(--sans);color:var(--ink);margin-top:8px}
.add-card span{font:400 12px var(--sans);color:var(--muted);margin-top:3px}

.badge{display:inline-block;font:600 10.5px var(--sans);padding:3px 9px;border-radius:20px;color:var(--muted);background:var(--chip)}
.badge.running,.badge.translating{color:var(--accent);background:var(--accentsoft)}
.badge.done,.badge.completed,.badge.succeeded{color:var(--good);background:var(--goodbg)}
.badge.failed,.badge.error{color:var(--danger);background:var(--accentsoft)}

/* wizard */
.wiz{display:flex;max-width:1040px;margin:0 auto;min-height:calc(100vh - 60px)}
.rail{width:236px;flex:none;padding:34px 24px;border-right:1px solid var(--line)}
.rail .kicker{font:500 10px var(--sans);text-transform:uppercase;letter-spacing:.07em;color:var(--faint);margin-bottom:16px}
.steps{display:flex;flex-direction:column;gap:4px}
.step{display:flex;align-items:center;gap:11px;padding:9px 10px;border-radius:10px;cursor:pointer}
.step.current{background:var(--card);box-shadow:0 1px 3px rgba(0,0,0,.07)}
.step .dot{width:23px;height:23px;flex:none;border-radius:50%;display:flex;align-items:center;justify-content:center;font:600 11px var(--mono);color:#fff;background:var(--faint)}
.step.current .dot{background:var(--accent)}
.step.done .dot{background:var(--good)}
.step .lbl{font:500 13px var(--sans);color:var(--muted)}
.step.current .lbl{font-weight:600;color:var(--ink)}
.step .hint{font:400 11px var(--sans);color:var(--faint)}
.wizsummary{margin-top:26px;padding:14px;border-radius:12px;background:var(--soft);border:1px solid var(--line)}
.wizsummary .kicker{margin-bottom:6px}
.wizsummary .t{font:600 14px/1.2 var(--serif);color:var(--ink)}
.wizsummary .m{font:400 11px var(--mono);color:var(--muted);margin-top:4px}
.wizpanel{flex:1;padding:34px 38px 40px;display:flex;flex-direction:column;animation:bf-fade .35s ease both}
.wizpanel .kicker{font:500 11px var(--sans);text-transform:uppercase;letter-spacing:.07em;color:var(--accent);margin-bottom:6px}
.wizpanel h2{font:600 25px/1.1 var(--serif);color:var(--ink);margin:0 0 5px}
.wizpanel .sub{font:400 13.5px var(--sans);color:var(--muted);margin-bottom:26px;max-width:560px}
.wizbody{flex:1}
.field-label{font:600 11px var(--sans);text-transform:uppercase;letter-spacing:.06em;color:var(--faint);margin-bottom:7px}
.drop{margin-top:6px;padding:26px 20px;border:1.5px dashed var(--line);border-radius:14px;background:var(--soft);text-align:center;cursor:pointer}
.drop.has{border-style:solid;border-color:var(--accentline);background:var(--card)}
.drop b{color:var(--accent)}
.drop .fname{font:600 14px var(--serif);color:var(--ink)}
.inp{width:100%;border-radius:10px;padding:12px 14px;background:var(--card);border:1px solid var(--line);font:500 14px var(--sans);color:var(--ink)}
.inp:focus{outline:none;border-color:var(--accent)}
.lang-row{display:flex;align-items:flex-end;gap:14px;margin-bottom:24px}
.lang-row .col{flex:1}
.swap{width:40px;height:40px;flex:none;border-radius:50%;background:var(--chip);display:flex;align-items:center;justify-content:center;color:var(--accent);font-size:16px;cursor:pointer;margin-bottom:4px}
.chips{display:flex;flex-wrap:wrap;gap:8px;margin-top:9px}
.chip{padding:8px 14px;border-radius:20px;cursor:pointer;font:500 12.5px var(--sans);color:var(--muted);background:var(--chip);border:1px solid transparent}
.chip.on{font-weight:600;color:var(--accentink);background:var(--accent);border-color:var(--accent)}
.tiers{display:flex;gap:12px;margin-bottom:20px}
.tier{flex:1;position:relative;padding:16px 15px;border-radius:13px;cursor:pointer;background:var(--card);border:1.5px solid var(--line)}
.tier.on{background:var(--accentsoft);border-color:var(--accent)}
.tier .tbadge{position:absolute;top:-9px;left:13px;font:600 9px var(--sans);text-transform:uppercase;letter-spacing:.05em;color:#fff;background:var(--good);padding:3px 8px;border-radius:5px;display:none}
.tier.rec .tbadge,.tier.on .tbadge{display:block}
.tier.on .tbadge{background:var(--accent)}
.tier .tn{font:600 14px var(--sans);color:var(--ink);margin-bottom:4px}
.tier .td{font:400 11.5px/1.4 var(--sans);color:var(--muted);margin-bottom:13px;min-height:48px}
.tier .tm{font:400 10.5px var(--mono);color:var(--faint);margin-top:4px}
.facts{display:grid;grid-template-columns:1fr 1fr;gap:11px;margin-bottom:13px}
.fact{padding:14px 15px;border:1px solid var(--line);border-radius:11px;background:var(--card)}
.fact .k{font:500 10px var(--sans);text-transform:uppercase;letter-spacing:.05em;color:var(--faint);margin-bottom:6px}
.fact .v{font:600 15px var(--sans);color:var(--ink)}
.fact .v.mono{font:600 13px var(--mono)}
.costbox{display:flex;align-items:center;justify-content:space-between;padding:17px 19px;background:var(--accentsoft);border:1px solid var(--accentline);border-radius:13px;margin-bottom:13px}
.costbox .ck{font:400 11px var(--sans);color:var(--muted);margin-bottom:3px}
.costbox .cv{font:600 28px var(--serif);color:var(--ink)}
.costbox .cm{text-align:right;font:400 12px/1.7 var(--mono);color:var(--muted)}
.advtoggle{display:flex;align-items:center;justify-content:space-between;padding:13px 4px;border-top:1px solid var(--line);border-bottom:1px solid var(--line);cursor:pointer;font:500 13px var(--sans);color:var(--muted)}
.advbody{padding:16px 2px}
.adv-grid{display:grid;grid-template-columns:1fr 1fr;gap:10px}
.adv-cell{display:flex;align-items:center;justify-content:space-between;padding:11px 13px;border:1px solid var(--line);border-radius:10px}
.adv-cell span{font:500 12.5px var(--sans);color:var(--muted)}
.adv-cell .val{font:600 12.5px var(--mono);color:var(--accent);cursor:pointer}
.stepper{display:flex;align-items:center;gap:6px}
.stepbtn{width:24px;height:24px;border-radius:6px;background:var(--chip);display:flex;align-items:center;justify-content:center;cursor:pointer;color:var(--ink)}
.numin{width:44px;text-align:center;border:1px solid var(--line);border-radius:7px;padding:4px;background:var(--card);color:var(--ink);font:600 12.5px var(--mono)}
.modelcards{display:grid;grid-template-columns:1fr 1fr;gap:8px;margin-bottom:9px}
.modelcard{display:flex;align-items:center;justify-content:space-between;gap:8px;padding:11px 13px;border-radius:11px;cursor:pointer;background:var(--card);border:1.5px solid var(--line)}
.modelcard.on{background:var(--accentsoft);border-color:var(--accent)}
.modelcard .ml{font:600 13px var(--sans);color:var(--ink);white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.modelcard .mi{font:400 10.5px var(--mono);color:var(--faint);white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.wizfoot{display:flex;gap:11px;margin-top:30px;padding-top:22px;border-top:1px solid var(--line);align-items:center}
.wizfoot .grow{flex:1}
.keyline{font:400 10px var(--sans);color:var(--faint);margin-top:4px}
.launchstatus{font:400 12px var(--sans);color:var(--muted);min-height:16px}

/* progress */
.prog-hero{display:flex;align-items:center;gap:14px;margin-bottom:22px}
.prog-cover{width:44px;height:62px;flex:none;border-radius:5px;display:flex;align-items:center;justify-content:center;font:600 20px var(--serif);color:var(--accentink);background:linear-gradient(150deg,var(--accent),var(--danger))}
.prog-hero .h{flex:1;min-width:0}
.prog-hero .h .t{font:600 22px/1.1 var(--serif);color:var(--ink);white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.prog-hero .h .m{font:400 12.5px var(--mono);color:var(--muted);margin-top:3px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.prog-card{padding:22px 24px;background:var(--card);border:1px solid var(--line);border-radius:16px;margin-bottom:16px}
.prog-top{display:flex;align-items:flex-end;justify-content:space-between;margin-bottom:13px}
.prog-pct{font:600 40px/1 var(--serif);color:var(--ink)}
.prog-pct small{font-size:20px;color:var(--muted)}
.prog-eta{text-align:right;font:400 12.5px/1.6 var(--sans);color:var(--muted)}
.prog-bar{height:10px;border-radius:6px;background:var(--chip);overflow:hidden}
.prog-bar > i{display:block;height:100%;border-radius:6px;background:repeating-linear-gradient(45deg,var(--accent) 0 10px,color-mix(in srgb,var(--accent) 78%,#000) 10px 20px);background-size:40px 40px;animation:bf-stripe 1s linear infinite;transition:width .3s ease}
.prog-bar.done > i{background:var(--good);animation:none}
.prog-actions{display:flex;gap:10px;margin-top:18px;align-items:center}
.stat-grid{display:grid;grid-template-columns:repeat(4,1fr);gap:11px;margin-bottom:16px}
.stat{padding:14px 15px;border:1px solid var(--line);border-radius:12px;background:var(--card)}
.stat .k{font:500 10px var(--sans);text-transform:uppercase;letter-spacing:.05em;color:var(--faint);margin-bottom:6px}
.stat .v{font:600 17px var(--mono);color:var(--ink)}
.stat .v.good{color:var(--good)} .stat .v.warn{color:var(--warn)} .stat .v.bad{color:var(--danger)}
.sectlabel{font:600 11px var(--sans);text-transform:uppercase;letter-spacing:.06em;color:var(--faint);margin:0 0 10px}
.logbox{background:var(--soft);border:1px solid var(--line);border-radius:12px;padding:8px 4px;max-height:230px;overflow:auto}
.logline{font:400 12px var(--mono);color:var(--muted);padding:6px 14px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.logline.good{color:var(--good)} .logline.warn{color:var(--warn)} .logline.bad{color:var(--danger)}
.live{display:flex;align-items:center;gap:6px;font:400 12px var(--sans);color:var(--muted)}
.dot{width:8px;height:8px;border-radius:50%;background:var(--faint)}
.dot.on{background:var(--good);box-shadow:0 0 8px var(--good)}

/* review */
.review{display:flex;height:calc(100vh - 60px)}
.rev-list{width:300px;flex:none;border-right:1px solid var(--line);display:flex;flex-direction:column}
.rev-list .lh{padding:18px 18px 12px}
.rev-list .lh .t{font:600 17px var(--serif);color:var(--ink)}
.rev-list .lh .m{font:400 11.5px var(--mono);color:var(--muted);margin-top:3px}
.rev-filters{display:flex;gap:6px;margin-top:12px;flex-wrap:wrap}
.rev-filter{padding:5px 11px;border-radius:16px;cursor:pointer;font:500 11.5px var(--sans);color:var(--muted);background:var(--chip)}
.rev-filter.on{font-weight:600;color:var(--accentink);background:var(--accent)}
.rev-rows{flex:1;overflow:auto}
.rev-row{padding:13px 18px;cursor:pointer;border-bottom:1px solid var(--line);border-left:3px solid transparent}
.rev-row.on{background:var(--soft);border-left-color:var(--accent)}
.rev-row .r{display:flex;align-items:center;gap:8px;margin-bottom:4px}
.rev-row .ref{font:600 10.5px var(--mono);color:var(--faint)}
.rev-tag{font:600 9px var(--sans);text-transform:uppercase;letter-spacing:.04em;padding:2px 7px;border-radius:10px;color:var(--good);background:var(--goodbg)}
.rev-tag.warn{color:var(--warn);background:var(--chip)}
.rev-tag.bad{color:var(--danger);background:var(--accentsoft)}
.rev-row .prev{font:400 12px/1.4 var(--sans);color:var(--muted);display:-webkit-box;-webkit-line-clamp:2;-webkit-box-orient:vertical;overflow:hidden}
.rev-row.on .prev{color:var(--ink)}
.rev-main{flex:1;display:flex;flex-direction:column;animation:bf-fade .35s ease both;min-width:0}
.rev-bar{display:flex;align-items:center;justify-content:space-between;padding:16px 24px;border-bottom:1px solid var(--line)}
.rev-bar .ref{font:600 13px var(--mono);color:var(--muted)}
.rev-nav{display:flex;gap:8px}
.rev-btn{width:34px;height:34px;border-radius:9px;border:1px solid var(--line);background:transparent;display:flex;align-items:center;justify-content:center;color:var(--muted);font-size:15px;cursor:pointer}
.rev-flag{padding:8px 14px;border-radius:9px;border:1px solid var(--line);background:transparent;color:var(--muted);font:600 12px var(--sans);cursor:pointer}
.rev-flag.on{border-color:var(--danger);color:var(--danger);background:var(--accentsoft)}
.rev-cols{flex:1;display:grid;grid-template-columns:1fr 1fr;overflow:auto}
.rev-col{padding:26px 28px;border-right:1px solid var(--line)}
.rev-col.tgt{background:var(--soft);border-right:none}
.rev-col .cl{font:600 10px var(--sans);text-transform:uppercase;letter-spacing:.07em;color:var(--faint);margin-bottom:14px}
.rev-col.tgt .cl{color:var(--accent)}
.rev-text{font:400 16px/1.7 var(--serif);color:var(--ink);white-space:pre-wrap}
.rev-note{margin-top:14px;padding:11px 13px;border-radius:9px;background:var(--card);border:1px solid var(--accentline);font:400 12.5px/1.5 var(--sans);color:var(--muted)}
.rev-note b{color:var(--warn)}
.rev-empty{flex:1;display:flex;align-items:center;justify-content:center;color:var(--muted);width:100%}

/* validation */
.val-hero{display:flex;align-items:center;gap:14px;margin-bottom:22px}
.val-icon{width:46px;height:46px;flex:none;border-radius:12px;display:flex;align-items:center;justify-content:center;font-size:22px}
.val-icon.good{color:var(--good);background:var(--goodbg)}
.val-icon.warn{color:var(--warn);background:var(--chip)}
.val-icon.bad{color:var(--danger);background:var(--accentsoft)}
.val-hero .h{flex:1;min-width:0}
.val-hero .h .t{font:600 24px/1.1 var(--serif);color:var(--ink)}
.val-hero .h .s{font:400 13px var(--sans);color:var(--muted);margin-top:4px}
.val-stats{display:grid;grid-template-columns:repeat(3,1fr);gap:11px;margin-bottom:20px}
.val-stat{padding:16px;border:1px solid var(--line);border-radius:13px;background:var(--card)}
.val-stat .v{font:600 26px var(--serif);color:var(--ink)}
.val-stat .v.good{color:var(--good)} .val-stat .v.warn{color:var(--warn)} .val-stat .v.bad{color:var(--danger)}
.val-stat .l{font:500 11.5px var(--sans);color:var(--muted);margin-top:3px}
.val-note{padding:12px 15px;border:1px solid var(--accentline);background:var(--accentsoft);border-radius:11px;margin-bottom:16px;font:400 12.5px var(--sans);color:var(--muted)}
.val-list{border:1px solid var(--line);border-radius:13px;overflow:hidden;background:var(--card)}
.val-item{display:flex;gap:13px;padding:14px 17px;border-bottom:1px solid var(--line)}
.val-item:last-child{border-bottom:none}
.val-dot{width:24px;height:24px;flex:none;border-radius:50%;display:flex;align-items:center;justify-content:center;font:600 12px var(--mono)}
.val-dot.good{color:var(--good);background:var(--goodbg)}
.val-dot.warn{color:var(--warn);background:var(--chip)}
.val-dot.bad{color:var(--danger);background:var(--accentsoft)}
.val-dot.info{color:var(--muted);background:var(--chip)}
.val-item .m{flex:1;min-width:0}
.val-item .mt{font:500 13.5px var(--sans);color:var(--ink)}
.val-item .ml{font:400 11.5px var(--mono);color:var(--faint);margin-top:3px;word-break:break-all}
.val-item .mc{font:500 11px var(--mono);color:var(--faint);white-space:nowrap}

/* glossary */
.gl-langs{display:flex;gap:12px;align-items:flex-end;margin:6px 0 18px}
.gl-langs > div{flex:1;max-width:220px}
.gl-add{display:flex;gap:9px;align-items:center;margin-bottom:12px}
.gl-status{font:400 12px var(--sans);color:var(--muted);min-height:16px;margin-bottom:6px}
.gl-table{border:1px solid var(--line);border-radius:13px;overflow:hidden;background:var(--card)}
.gl-head,.gl-row{display:grid;grid-template-columns:1fr 1fr 120px 40px;align-items:center;padding:12px 17px;gap:10px}
.gl-head{background:var(--soft);border-bottom:1px solid var(--line);font:600 10px var(--sans);text-transform:uppercase;letter-spacing:.05em;color:var(--faint)}
.gl-row{border-bottom:1px solid var(--line)}
.gl-row:last-of-type{border-bottom:none}
.gl-c.s{font:500 14px var(--sans);color:var(--ink)}
.gl-c.t{font:400 14px var(--serif);color:var(--ink)}
.gl-c.cat{font:400 11.5px var(--mono);color:var(--muted)}
.gl-c.x{text-align:right;color:var(--faint);cursor:pointer;font-size:16px}
.gl-c.x:hover{color:var(--danger)}
.gl-foot{padding:11px 17px;font:400 12px var(--sans);color:var(--faint);border-top:1px solid var(--line)}
[hidden]{display:none !important}
</style>
</head>
<body>
<header class="appbar">
  <div class="brand" onclick="bfGo('library')"><b>BookForge</b><small>v2</small></div>
  <nav class="nav" id="nav"></nav>
  <span class="spacer"></span>
  <div class="right">
    <button class="btn btn-primary" onclick="bfStartNew()">+ New translation</button>
    <div class="themetoggle" onclick="bfTheme()"><span id="sun">&#9728;</span><span id="moon">&#9790;</span></div>
  </div>
</header>
<main id="stage"></main>

<script>
const CSRF_HEADER = "x-bookforge-csrf";
const CSRF_TOKEN = "__BOOKFORGE_CSRF_TOKEN__";
const $ = (sel, el) => (el || document).querySelector(sel);
const ESC = { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" };
function esc(value) { return String(value == null ? "" : value).replace(/[&<>"']/g, ch => ESC[ch]); }
function num(n) { return (n || 0).toLocaleString(); }
function pct(done, total) { return total > 0 ? Math.min(100, Math.round(done / total * 100)) : 0; }
function shorten(s, n) { s = s || ""; return s.length > n ? s.slice(0, n - 1) + "…" : s; }
function badgeClass(status) { return (status || "").toLowerCase().replace(/[^a-z]/g, ""); }
function titleFromPath(path) {
  let base = String(path || "").split(/[\\/]/).pop() || "book";
  base = base.replace(/\.epub$/i, "").replace(/^\d{6,}-/, "").replace(/[._-]+/g, " ").trim();
  return base ? base.charAt(0).toUpperCase() + base.slice(1) : "Untitled";
}
function fmtDur(secs) {
  secs = Math.round(secs);
  if (secs <= 0) return "—";
  const h = Math.floor(secs / 3600), m = Math.floor((secs % 3600) / 60), s = secs % 60;
  return h > 0 ? `${h}h${String(m).padStart(2,"0")}m` : (m > 0 ? `${m}m${String(s).padStart(2,"0")}s` : `${s}s`);
}
function elapsedSecs(s) {
  if (s.finished && s.finished_elapsed_ms != null) return s.finished_elapsed_ms / 1000;
  if (s.first_timestamp_ms != null && s.last_timestamp_ms != null && s.last_timestamp_ms >= s.first_timestamp_ms)
    return (s.last_timestamp_ms - s.first_timestamp_ms) / 1000;
  return 0;
}
function segPerMin(s) { const e = elapsedSecs(s); return e > 0 ? (s.done_segments || 0) / e * 60 : 0; }
function etaSecs(s) { const r = Math.max(0, (s.total_segments || 0) - (s.done_segments || 0)); const pm = segPerMin(s); return pm > 0 ? r / (pm / 60) : 0; }
function fmtCost(v) { return v == null ? "n/a" : "$" + Number(v).toFixed(2); }

const QUALITY = [
  { id:"economy",  name:"Economy",  desc:"Fast, very cheap. Good for drafts.", profile:"fastest",  provider:"deepseek", model:"deepseek-v4-flash" },
  { id:"balanced", name:"Balanced", desc:"Strong quality, sensible price.",   profile:"balanced", provider:"deepseek", model:"deepseek-v4-flash", rec:true },
  { id:"finest",   name:"Finest",   desc:"Top models, literary register.",    profile:"safe",     provider:"openrouter", model:"openrouter/auto" },
];

const App = {
  screen: "library",
  theme: localStorage.getItem("bf-theme") || "light",
  jobs: [],
  selected: null,
  es: null,
  options: { languages: ["English","Italian","Spanish","French","German"], providers: [] },
  providerKeys: {},
  wizard: null,
};

function freshWizard() {
  return { step:0, file:null, fileName:"", from:"", to:"Italian", quality:"balanced",
    provider:"deepseek", model:"deepseek-v4-flash", profile:"balanced",
    advancedOpen:false, concurrency:4, qa:"suspicious", context:3, validate:false,
    apiKey:"", baseUrl:"", estimate:null, status:"" };
}

function applyTheme() {
  document.documentElement.setAttribute("data-theme", App.theme);
  $("#sun").classList.toggle("on", App.theme === "light");
  $("#moon").classList.toggle("on", App.theme === "dark");
}
function bfTheme() { App.theme = App.theme === "dark" ? "light" : "dark"; localStorage.setItem("bf-theme", App.theme); applyTheme(); }

function bfGo(screen, opts) { Object.assign(App, opts || {}); App.screen = screen; if (screen !== "progress") closeStream(); render(); }
function bfStartNew() { App.wizard = freshWizard(); App.screen = "wizard"; closeStream(); render(); }

const NAV = [["library","Library"],["progress","Progress"],["review","Review"],["validation","Validation"],["glossary","Glossary"]];
function renderNav() {
  const active = App.screen === "wizard" ? "library" : App.screen;
  $("#nav").innerHTML = NAV.map(([id,label]) =>
    `<div class="tab ${id===active?"active":""}" onclick="bfGo('${id}')">${label}</div>`).join("");
}

function render() {
  renderNav();
  const stage = $("#stage");
  switch (App.screen) {
    case "library": return renderLibrary(stage);
    case "wizard": return renderWizard(stage);
    case "progress": return renderProgress(stage);
    case "review": return renderReview(stage);
    case "validation": return renderValidation(stage);
    case "glossary": return renderGlossary(stage);
    default: return renderLibrary(stage);
  }
}

/* ---------------- Library ---------------- */
function jobDone(st) { return st === "succeeded" || st === "done" || st === "completed"; }
async function renderLibrary(stage) {
  stage.innerHTML = `<div class="wrap">
    <div class="pagehead"><div><h1>Your library</h1><p>Pick up a translation, review a finished book, or start a new one.</p></div>
      <button class="btn btn-primary" onclick="bfStartNew()">+ New translation</button></div>
    <div class="book-grid" id="grid"><div class="empty">Loading…</div></div></div>`;
  loadLibraryJobs();
}
async function loadLibraryJobs() {
  let jobs = [];
  try { jobs = await (await fetch("/api/jobs")).json(); } catch (e) { jobs = []; }
  App.jobs = jobs;
  const grid = $("#grid");
  if (!grid || App.screen !== "library") return;
  const cards = jobs.map(j => {
    const p = pct(j.done, j.total_segments);
    const st = badgeClass(j.status);
    const done = jobDone(st);
    const action = done ? "Review →" : (st === "failed" || st === "error") ? "Inspect →" : (p > 0 ? "View progress →" : "Open →");
    const title = titleFromPath(j.input_path);
    return `<div class="book-card" onclick="bfOpenJob('${esc(j.id)}','${st}')">
      <div class="cover">${esc(title.charAt(0))}</div>
      <div class="book-main">
        <div class="book-title">${esc(title)}</div>
        <div class="book-sub">${esc(j.provider)} / ${esc(j.model)}</div>
        <div class="book-meta"><span class="badge ${st}">${esc(j.status)}</span><span class="mono">${j.done}/${j.total_segments} · ${esc(j.target_lang)}</span></div>
        <div class="bar-track" ${p?"":'style="opacity:0"'}><div class="bar-fill" style="width:${p}%;${done?"background:var(--good)":""}"></div></div>
      </div>
      <div class="book-action">${action}</div></div>`;
  }).join("");
  grid.innerHTML = cards + `<div class="add-card" onclick="bfStartNew()">
      <div class="plus">＋</div><b>Translate a new book</b><span>Drop an EPUB to begin</span></div>`;
}
function bfOpenJob(id, st) {
  if (jobDone(st)) bfGo("review", { selected: id });
  else bfGo("progress", { selected: id });
}

/* ---------------- Wizard ---------------- */
const WIZ_STEPS = [
  { label:"Book", hint:"Source file" },
  { label:"Languages", hint:"Pair" },
  { label:"Quality", hint:"Tier" },
  { label:"Review & start", hint:"Confirm plan" },
];
const WIZ_META = [
  ["Step 1 · Your book","Pick the source file","This is the EPUB BookForge will translate. Structure, footnotes and code blocks are protected."],
  ["Step 2 · Languages","Choose the pair","Pick the source (or leave it to auto-detect) and the target language."],
  ["Step 3 · Quality","How good, how cheap","Sets the model and pricing tier. Fine-tune the exact model under Advanced on the next step."],
  ["Step 4 of 4 · Review","Ready when you are","Review the plan, then start. The job is checkpointed every chapter, so you can resume or retry anytime."],
];
function providerOption(id) { return (App.options.providers || []).find(p => p.id === id) || (App.options.providers || [])[0] || { id:"mock", models:[], requires_key:false, requires_base_url:false }; }

function renderWizard(stage) {
  const w = App.wizard || (App.wizard = freshWizard());
  const meta = WIZ_META[w.step];
  const rail = WIZ_STEPS.map((st,i) => {
    const cls = i < w.step ? "done" : i === w.step ? "current" : "";
    return `<div class="step ${cls}" onclick="bfWizGo(${i})"><span class="dot">${i<w.step?"✓":i+1}</span>
      <div style="flex:1"><div class="lbl">${st.label}</div><div class="hint">${st.hint}</div></div></div>`;
  }).join("");
  stage.innerHTML = `<div class="wiz">
    <div class="rail"><div class="kicker">New translation</div><div class="steps">${rail}</div>
      <div class="wizsummary"><div class="kicker">Translating</div>
        <div class="t">${esc(w.fileName ? titleFromPath(w.fileName) : "No file yet")}</div>
        <div class="m">${esc((w.from||"auto"))} → ${esc(w.to||"?")} · ${esc(qualityName(w.quality))}</div></div></div>
    <div class="wizpanel"><div class="kicker">${meta[0]}</div><h2>${meta[1]}</h2><div class="sub">${meta[2]}</div>
      <div class="wizbody" id="wizbody"></div>
      <div class="wizfoot">
        <button class="btn btn-ghost" onclick="bfWizBack()" ${w.step===0?"hidden":""}>Back</button>
        <span class="grow"></span><span class="launchstatus" id="launchstatus">${esc(w.status||"")}</span>
        <button class="btn btn-primary" id="wiznext" style="padding:13px 26px;font-size:14px" onclick="bfWizNext()">${w.step===3?"Start translation":"Continue"}</button>
      </div></div></div>`;
  renderWizBody();
}
function qualityName(id) { const q = QUALITY.find(q => q.id === id); return q ? q.name : id; }
function bfWizGo(i) { syncWizInputs(); App.wizard.step = Math.max(0, Math.min(3, i)); renderWizard($("#stage")); }
function bfWizBack() { syncWizInputs(); if (App.wizard.step > 0) { App.wizard.step--; renderWizard($("#stage")); } }

function renderWizBody() {
  const w = App.wizard, body = $("#wizbody"); if (!body) return;
  if (w.step === 0) {
    body.innerHTML = `<div class="drop ${w.file?"has":""}" onclick="$('#fileinput').click()">
      ${w.file ? `<div class="fname">${esc(w.fileName)}</div><div style="color:var(--muted);font-size:12px;margin-top:6px">Click to choose a different file</div>`
               : `<div>Drop an <b>EPUB</b> here or click to browse.</div>`}
      </div><input type="file" id="fileinput" accept=".epub" hidden onchange="bfPickFile(this)">`;
  } else if (w.step === 1) {
    const chips = ["Italian","Spanish","French","German","Japanese","Korean","Portuguese","Chinese (Simplified)"];
    body.innerHTML = `<div class="lang-row">
        <div class="col"><div class="field-label">Translate from</div>
          <input class="inp" id="w_from" list="langs" placeholder="Auto-detect" value="${esc(w.from)}"></div>
        <div class="swap" onclick="bfSwapLangs()">⇄</div>
        <div class="col"><div class="field-label">Into</div>
          <input class="inp" id="w_to" list="langs" placeholder="Type a language…" value="${esc(w.to)}"></div>
      </div>
      <datalist id="langs">${(App.options.languages||[]).map(l=>`<option value="${esc(l)}">`).join("")}</datalist>
      <div class="field-label">Quick pick</div>
      <div class="chips">${chips.map(n=>`<div class="chip ${w.to===n?"on":""}" onclick="bfPickTo('${esc(n)}')">${esc(n)}</div>`).join("")}</div>`;
  } else if (w.step === 2) {
    body.innerHTML = `<div class="tiers">${QUALITY.map(q=>`
      <div class="tier ${w.quality===q.id?"on":""} ${q.rec?"rec":""}" onclick="bfPickTier('${q.id}')">
        <div class="tbadge">${w.quality===q.id?"Selected":q.rec?"Recommended":""}</div>
        <div class="tn">${q.name}</div><div class="td">${q.desc}</div>
        <div class="tm">${esc(q.provider)} · ${esc(q.model)}</div></div>`).join("")}</div>
      <p style="font:400 12.5px var(--sans);color:var(--muted)">You can override the provider and exact model under <b>Advanced</b> on the next step — including the offline <b>mock</b> provider for a dry run.</p>`;
  } else {
    renderReviewStep(body);
  }
}
function syncWizInputs() {
  const w = App.wizard; if (!w) return;
  const from = $("#w_from"); if (from) w.from = from.value.trim();
  const to = $("#w_to"); if (to) w.to = to.value.trim();
  const key = $("#w_key"); if (key) w.apiKey = key.value;
  const base = $("#w_base"); if (base) w.baseUrl = base.value.trim();
  const conc = $("#w_conc"); if (conc) w.concurrency = Math.max(1, Math.min(16, parseInt(conc.value,10) || 1));
  const mid = $("#w_modelid"); if (mid && mid.value.trim()) w.model = mid.value.trim();
}
function bfPickFile(input) {
  const f = input.files && input.files[0];
  if (!f) return;
  App.wizard.file = f; App.wizard.fileName = f.name; App.wizard.estimate = null;
  renderWizard($("#stage"));
}
function bfSwapLangs() { syncWizInputs(); const w = App.wizard; const t = w.from; w.from = w.to; w.to = t; renderWizBody(); }
function bfPickTo(name) { syncWizInputs(); App.wizard.to = name; renderWizBody(); }
function bfPickTier(id) {
  const q = QUALITY.find(q => q.id === id); if (!q) return;
  const w = App.wizard; w.quality = id; w.profile = q.profile; w.provider = q.provider; w.model = q.model;
  w.estimate = null; renderWizBody();
}

function renderReviewStep(body) {
  const w = App.wizard;
  const opt = providerOption(w.provider);
  const needsKey = opt.requires_key === true && App.providerKeys[w.provider] !== true;
  const needsBase = opt.requires_base_url === true;
  const facts = [
    { k:"Languages", v:`${w.from||"auto"} → ${w.to||"?"}` },
    { k:"Quality", v:qualityName(w.quality) },
    { k:"Model", v:`${esc(w.provider)} · ${esc(w.model)}`, mono:true },
    { k:"Profile", v:esc(w.profile) },
  ];
  const est = w.estimate;
  const costLabel = est ? fmtCost(est.cost_usd) : (w.file ? "…" : "add a file");
  const tokens = est ? num(est.input_tokens + est.output_tokens) : "—";
  const providerChips = (App.options.providers||[]).map(p =>
    `<div class="chip ${w.provider===p.id?"on":""}" onclick="bfPickProvider('${p.id}')">${esc(p.label||p.id)}</div>`).join("");
  const models = (opt.models||[]).map(m =>
    `<div class="modelcard ${w.model===m?"on":""}" onclick="bfPickModel('${esc(m)}')"><div style="min-width:0"><div class="ml">${esc(m)}</div></div></div>`).join("");
  body.innerHTML = `
    <div class="facts">${facts.map(f=>`<div class="fact"><div class="k">${f.k}</div><div class="v ${f.mono?"mono":""}">${f.v}</div></div>`).join("")}</div>
    <div class="costbox"><div><div class="ck">Estimated cost</div><div class="cv" id="costv">${costLabel}</div></div>
      <div class="cm"><span id="esttokens">${tokens}</span> tokens<br>${est?"priced from catalog":"parses your EPUB"}</div></div>
    <div class="advtoggle" onclick="bfToggleAdvanced()"><span><span style="color:var(--accent)">⚙</span> Advanced — provider, model, concurrency, QA, context, validation</span><span>${w.advancedOpen?"▾":"▸"}</span></div>
    <div class="advbody" ${w.advancedOpen?"":"hidden"}>
      <div class="field-label">Provider</div>
      <div class="chips" style="margin-bottom:15px">${providerChips}</div>
      ${needsBase?`<div class="field-label">Base URL</div><input class="inp" id="w_base" placeholder="https://api.example.com/v1" value="${esc(w.baseUrl)}" style="margin-bottom:15px">`:""}
      ${needsKey?`<div class="field-label">API key</div><input class="inp" id="w_key" type="password" autocomplete="off" placeholder="Paste once for this session" value="${esc(w.apiKey)}"><div class="keyline">Read from the server environment or remembered for this server session.</div>`:""}
      <div class="field-label" style="margin-top:15px">Model · ${esc((opt.label||opt.id))}</div>
      <div class="modelcards">${models||`<div style="color:var(--faint);font-size:12px;padding:8px">No preset models</div>`}</div>
      <div style="display:flex;align-items:center;gap:8px;margin:2px 0 6px">
        <span style="font:400 11px var(--sans);color:var(--faint);white-space:nowrap">Or type any ID</span>
        <input class="inp" id="w_modelid" placeholder="provider/model-name" value="${esc(w.model)}" oninput="App.wizard.model=this.value.trim();App.wizard.estimate=null" onchange="requestEstimate()"></div>
      <div class="adv-grid" style="margin-top:6px">
        <div class="adv-cell"><span>Concurrency</span><div class="stepper">
          <div class="stepbtn" onclick="bfConc(-1)">−</div>
          <input class="numin" id="w_conc" type="number" min="1" max="16" value="${w.concurrency}">
          <div class="stepbtn" onclick="bfConc(1)">+</div></div></div>
        <div class="adv-cell" onclick="bfCycleQa()"><span>QA pass</span><span class="val">${esc(w.qa)}</span></div>
        <div class="adv-cell" onclick="bfCycleContext()"><span>Context window</span><span class="val">${w.context}</span></div>
        <div class="adv-cell" onclick="bfToggleValidate()"><span>Validate output</span><span class="val" style="${w.validate?"color:var(--good)":""}">${w.validate?"On":"Off"}</span></div>
      </div>
    </div>`;
  if (w.file && !w.estimate) requestEstimate();
}
function bfToggleAdvanced() { syncWizInputs(); App.wizard.advancedOpen = !App.wizard.advancedOpen; renderReviewStep($("#wizbody")); }
function bfPickProvider(id) { syncWizInputs(); const w = App.wizard; w.provider = id; const opt = providerOption(id); w.model = opt.default_model || (opt.models||[])[0] || w.model; w.estimate = null; renderReviewStep($("#wizbody")); }
function bfPickModel(m) { syncWizInputs(); App.wizard.model = m; App.wizard.estimate = null; renderReviewStep($("#wizbody")); }
function bfConc(d) { const el = $("#w_conc"); if (!el) return; let v = (parseInt(el.value,10)||1) + d; v = Math.max(1, Math.min(16, v)); el.value = v; App.wizard.concurrency = v; }
function bfCycleQa() { const w = App.wizard; w.qa = w.qa === "off" ? "suspicious" : w.qa === "suspicious" ? "all" : "off"; renderReviewStep($("#wizbody")); }
function bfCycleContext() { const w = App.wizard; w.context = w.context >= 6 ? 0 : w.context + 1; renderReviewStep($("#wizbody")); }
function bfToggleValidate() { App.wizard.validate = !App.wizard.validate; renderReviewStep($("#wizbody")); }

async function requestEstimate() {
  const w = App.wizard; if (!w.file) return;
  const fd = new FormData();
  fd.append("file", w.file); fd.append("provider", w.provider);
  if (w.model) fd.append("model", w.model);
  if (w.to) fd.append("target", w.to);
  try {
    const r = await fetch("/api/estimate", { method: "POST", headers: { [CSRF_HEADER]: CSRF_TOKEN }, body: fd });
    const j = await r.json();
    if (!r.ok) return;
    if (App.screen === "wizard" && App.wizard === w) {
      w.estimate = j;
      const cv = $("#costv"); if (cv) cv.textContent = fmtCost(j.cost_usd);
      const et = $("#esttokens"); if (et) et.textContent = num(j.input_tokens + j.output_tokens);
    }
  } catch (e) {}
}

async function bfWizNext() {
  syncWizInputs();
  const w = App.wizard;
  if (w.step === 0) { if (!w.file) { toastWiz("choose an EPUB file"); return; } w.step = 1; return renderWizard($("#stage")); }
  if (w.step === 1) { if (!w.to) { toastWiz("choose a target language"); return; } w.step = 2; return renderWizard($("#stage")); }
  if (w.step === 2) { w.step = 3; return renderWizard($("#stage")); }
  return launchTranslation();
}
function toastWiz(msg) { const el = $("#launchstatus"); if (el) el.textContent = msg; if (App.wizard) App.wizard.status = msg; }

async function launchTranslation() {
  const w = App.wizard;
  const opt = providerOption(w.provider);
  if (opt.requires_base_url && !w.baseUrl) { w.advancedOpen = true; renderWizBody(); return toastWiz("base URL is required for this provider"); }
  if (w.launching) return;
  w.launching = true;
  const btn = $("#wiznext");
  const reenable = () => { w.launching = false; if (btn) { btn.disabled = false; btn.style.opacity = ""; btn.textContent = "Start translation"; } };
  if (btn) { btn.disabled = true; btn.style.opacity = ".6"; btn.textContent = "Starting…"; }
  toastWiz("uploading…");
  const fd = new FormData();
  fd.append("file", w.file);
  fd.append("target", w.to);
  if (w.from) fd.append("source", w.from);
  fd.append("provider", w.provider);
  if (w.model) fd.append("model", w.model);
  fd.append("profile", w.profile);
  fd.append("concurrency", String(w.concurrency));
  fd.append("qa", w.qa);
  fd.append("context_window", String(w.context));
  if (w.validate) fd.append("validate_output", "true");
  if (w.apiKey) fd.append("api_key", w.apiKey);
  if (w.baseUrl) fd.append("base_url", w.baseUrl);
  try {
    const r = await fetch("/api/translate", { method: "POST", headers: { [CSRF_HEADER]: CSRF_TOKEN }, body: fd });
    const j = await r.json();
    if (!r.ok) { reenable(); toastWiz(j.error || "launch failed"); return; }
    toastWiz("started — locating job…");
    await loadProviderStatus();
    trySelectPending(j.input_path, 0);
  } catch (e) { reenable(); toastWiz("launch failed"); }
}
async function trySelectPending(inputPath, attempt) {
  if (attempt > 25) return;
  let jobs = [];
  try { jobs = await (await fetch("/api/jobs")).json(); } catch (e) {}
  const match = jobs.find(j => j.input_path === inputPath);
  if (match) { bfGo("progress", { selected: match.id }); return; }
  setTimeout(() => trySelectPending(inputPath, attempt + 1), 900);
}

/* ---------------- Progress ---------------- */
async function renderProgress(stage) {
  const id = App.selected;
  if (!id) { stage.innerHTML = `<div class="wrap"><div class="empty">Open a translation from the library to watch its progress.</div></div>`; return; }
  stage.innerHTML = `<div class="wrap"><div class="empty">Loading job…</div></div>`;
  let d;
  try { const r = await fetch("/api/jobs/" + encodeURIComponent(id)); if (!r.ok) throw new Error(); d = await r.json(); }
  catch (e) { stage.innerHTML = `<div class="wrap"><div class="empty">Could not load this job.</div></div>`; return; }
  const title = titleFromPath(d.input_path);
  stage.innerHTML = `<div class="wrap">
    <div class="prog-hero"><div class="prog-cover">${esc(title.charAt(0))}</div>
      <div class="h"><div class="t">${esc(d.id)}</div>
        <div class="m">${esc(d.provider)} / ${esc(d.model)} · ${esc(d.source_lang||"auto")} → ${esc(d.target_lang)}</div></div>
      <span class="badge ${badgeClass(d.status)}" id="progpill">${esc(d.status)}</span></div>
    <div class="prog-card">
      <div class="prog-top"><div class="prog-pct"><span id="pctv">0</span><small>%</small></div>
        <div class="prog-eta" id="etav"></div></div>
      <div class="prog-bar" id="progbar"><i id="barfill" style="width:0%"></i></div>
      <div class="prog-actions"><span class="live"><span class="dot" id="livedot"></span><span id="livetxt">connecting…</span></span>
        <span class="grow" style="flex:1"></span>
        <button class="btn btn-ghost" onclick="bfGo('review',{selected:'${esc(d.id)}'})">Open review →</button>
        <button class="btn btn-ghost" id="retrybtn" onclick="bfRetry('${esc(d.id)}')">Retry failed / needs-review</button></div></div>
    <div class="stat-grid" id="stats"></div>
    <p class="sectlabel">Live activity</p>
    <div class="logbox scr" id="events"><div class="logline">waiting…</div></div>
    <div style="margin-top:16px"><p class="sectlabel">Issues</p><div class="logbox scr" id="issues"><div class="logline">none</div></div></div>
    <span class="toast" id="toast" style="font:400 12px var(--sans);color:var(--muted)"></span>
    </div>`;
  updateState(d.state || {});
  openStream(id);
}
function setLive(on, txt) { const dt = $("#livedot"), tx = $("#livetxt"); if (dt) dt.classList.toggle("on", on); if (tx) tx.textContent = txt; }
function updateState(s) {
  const total = s.total_segments || 0, done = s.done_segments || 0, p = pct(done, total);
  const fill = $("#barfill"); if (fill) fill.style.width = p + "%";
  const pv = $("#pctv"); if (pv) pv.textContent = p;
  const bar = $("#progbar"); if (bar) bar.classList.toggle("done", !!s.finished);
  const etav = $("#etav"); if (etav) etav.innerHTML = `${done} / ${total} segments<br>${s.finished ? "Finished" : "about " + fmtDur(etaSecs(s)) + " remaining"}`;
  const stats = [
    ["done", num(done), ""], ["succeeded", num(s.succeeded || 0), "good"], ["cached", num(s.cached || 0), ""],
    ["needs review", num(s.needs_review || 0), s.needs_review ? "warn" : ""],
    ["failed", num(s.failed || 0), s.failed ? "bad" : ""],
    ["active", `${s.active_requests || 0}/${s.target_concurrency || 0}`, ""],
    ["seg/min", segPerMin(s).toFixed(1), ""], ["elapsed", fmtDur(elapsedSecs(s)), ""],
    ["tokens in", num(s.input_tokens), ""], ["tokens out", num(s.output_tokens), ""],
  ];
  const box = $("#stats"); if (box) box.innerHTML = stats.map(([k,v,c]) => `<div class="stat"><div class="k">${esc(k)}</div><div class="v ${c}">${esc(v)}</div></div>`).join("");
  const ibox = $("#issues");
  if (ibox) { const issues = s.recent_issues || [];
    ibox.innerHTML = issues.length ? issues.slice().reverse().map(i => `<div class="logline ${i.level==="Error"?"bad":"warn"}">${i.level==="Error"?"✗":"⚠"} ${esc(i.kind)}: ${esc(shorten(i.message,120))}</div>`).join("") : `<div class="logline">none</div>`;
  }
  const ebox = $("#events");
  if (ebox) { const evs = s.recent_events || [];
    ebox.innerHTML = evs.length ? evs.slice().reverse().map(fmtEvent).join("") : `<div class="logline">waiting…</div>`;
  }
}
function fmtEvent(ev) {
  const key = Object.keys(ev)[0]; const v = ev[key] || {}; let cls = "", body = key;
  switch (key) {
    case "SegmentFinished": body = `segment ${shorten(v.segment_id,18)} → ${v.status}`; if (v.status==="failed") cls="bad"; else if (v.status==="needs_review") cls="warn"; break;
    case "SegmentStarted": body = `segment ${shorten(v.segment_id,18)} started`; break;
    case "RequestStarted": body = `request started (${v.active_requests}/${v.target_concurrency})`; break;
    case "RequestFinished": body = `request ${v.status} · ${v.latency_ms}ms`; if (v.status!=="ok"&&v.status!=="succeeded") cls="warn"; break;
    case "StageStarted": body = `stage: ${v.stage}`; break;
    case "StageFinished": body = `stage complete: ${v.stage}`; break;
    case "SegmentationFinished": body = `segmented into ${v.segment_count} segments`; break;
    case "CacheScanFinished": body = `cache scan: ${v.hits} hits / ${v.misses} misses`; break;
    case "CheckpointFlushed": body = `checkpoint flushed (${v.flushed_count})`; break;
    case "ConcurrencyChanged": body = `concurrency ${v.previous} → ${v.current} (${v.reason})`; break;
    case "Warning": body = `⚠ ${v.kind}: ${shorten(v.message,90)}`; cls="warn"; break;
    case "Error": body = `✗ ${v.kind}: ${shorten(v.message,90)}`; cls="bad"; break;
    case "TranslationFinished": body = `finished: ${v.succeeded} ok, ${v.cached} cached, ${v.needs_review} review, ${v.failed} failed`; cls="good"; break;
  }
  return `<div class="logline ${cls}">${esc(body)}</div>`;
}
function openStream(id) {
  closeStream();
  App.es = new EventSource("/api/jobs/" + encodeURIComponent(id) + "/events");
  setLive(true, "live");
  App.es.addEventListener("state", (e) => { if (App.selected === id && App.screen === "progress") { try { updateState(JSON.parse(e.data)); } catch (_) {} } });
  App.es.addEventListener("done", () => { setLive(false, "finished"); closeStream(); });
  App.es.onerror = () => setLive(false, "reconnecting…");
}
function closeStream() { if (App.es) { App.es.close(); App.es = null; } }
async function bfRetry(id) {
  const btn = $("#retrybtn"), toast = $("#toast");
  if (btn) btn.disabled = true; if (toast) toast.textContent = "submitting…";
  try {
    const r = await fetch("/api/jobs/" + encodeURIComponent(id) + "/retry", { method: "POST", headers: { [CSRF_HEADER]: CSRF_TOKEN } });
    const j = await r.json();
    if (toast) toast.textContent = r.ok ? `marked ${j.retried} segment(s) — run: bookforge resume ${id}` : (j.error || "retry failed");
  } catch (e) { if (toast) toast.textContent = "retry failed"; }
  if (btn) btn.disabled = false;
}

/* ---------------- Review / Validation / Glossary (wired in later milestones) ---------------- */
function placeholder(stage, title, note) {
  stage.innerHTML = `<div class="wrap"><div class="pagehead"><div><h1>${title}</h1><p>${note}</p></div></div>
    <div class="empty">${App.selected ? "Loading…" : "Open a job from the library first."}</div></div>`;
}
function flagKey(id) { return `bookforge.review.flags.${id}`; }
function loadFlags(id) { try { return JSON.parse(localStorage.getItem(flagKey(id)) || "{}"); } catch (e) { return {}; } }
function saveFlags(id, flags) { try { localStorage.setItem(flagKey(id), JSON.stringify(flags)); } catch (e) {} }
function segTag(seg, flagged) {
  if (flagged) return { label:"Flagged", cls:"bad" };
  if (seg.status === "failed") return { label:"failed", cls:"bad" };
  if (seg.status === "needs_review") return { label:"review", cls:"warn" };
  if ((seg.soft_warnings || []).length) return { label:"check", cls:"warn" };
  return { label:"ok", cls:"" };
}
async function renderReview(stage) {
  const id = App.selected;
  if (!id) { placeholder(stage, "Review", "Side-by-side source and translation."); return; }
  stage.innerHTML = `<div class="review"><div class="rev-empty">Loading review…</div></div>`;
  let doc;
  try {
    const r = await fetch("/api/jobs/" + encodeURIComponent(id) + "/review");
    doc = await r.json();
    if (!r.ok) { stage.innerHTML = `<div class="wrap"><div class="empty">${esc(doc.error || "Review is not available for this job.")}</div></div>`; return; }
  } catch (e) { stage.innerHTML = `<div class="wrap"><div class="empty">Could not load review.</div></div>`; return; }
  App.review = { doc, idx: 0, filter: "all", flags: loadFlags(id) };
  drawReview();
}
function bfReviewPick(i) { App.review.idx = i; drawReview(); }
function bfReviewNav(d) { const n = (App.review.doc.segments || []).length; App.review.idx = Math.max(0, Math.min(n - 1, App.review.idx + d)); drawReview(); }
function bfReviewFilter(f) { App.review.filter = f; drawReview(); }
function bfReviewFlag() {
  const R = App.review, seg = R.doc.segments[R.idx]; if (!seg) return;
  if (R.flags[seg.segment_id]) delete R.flags[seg.segment_id]; else R.flags[seg.segment_id] = { kind: "flagged" };
  saveFlags(App.selected, R.flags); drawReview();
}
function drawReview() {
  const R = App.review, doc = R.doc, segs = doc.segments || [];
  const flaggedCount = Object.keys(R.flags).length;
  const visible = segs.map((s, i) => ({ s, i })).filter(({ s }) => {
    if (R.filter === "flagged") return !!R.flags[s.segment_id];
    if (R.filter === "warnings") return (s.soft_warnings || []).length || (s.status !== "succeeded" && s.status !== "skipped_cached");
    return true;
  });
  const filters = [["all", `All ${segs.length}`], ["warnings", "To check"], ["flagged", `Flagged ${flaggedCount}`]];
  const rows = visible.map(({ s, i }) => {
    const flagged = !!R.flags[s.segment_id];
    const tag = segTag(s, flagged);
    const ref = `${s.chapter_title || s.chapter_id} ¶${s.ordinal}`;
    return `<div class="rev-row ${i === R.idx ? "on" : ""}" onclick="bfReviewPick(${i})">
      <div class="r"><span class="ref">${esc(shorten(ref, 24))}</span><span class="rev-tag ${tag.cls}">${tag.label}</span></div>
      <div class="prev">${esc(shorten(s.target_text || "—", 150))}</div></div>`;
  }).join("") || `<div style="padding:18px;color:var(--faint);font-size:12px">Nothing here.</div>`;
  const cur = segs[R.idx];
  const title = doc.source_book_title || titleFromPath(App.selected);
  const langs = `${esc(doc.source_language || "auto")} → ${esc(doc.target_language)}`;
  let main;
  if (!cur) {
    main = `<div class="rev-empty">No translated segments yet.</div>`;
  } else {
    const flagged = !!R.flags[cur.segment_id];
    const ref = `${cur.chapter_title || cur.chapter_id} ¶${cur.ordinal}`;
    const notes = (cur.soft_warnings || []).map(w =>
      `<div class="rev-note"><b>⚑ ${esc((w.kind || "note").replace(/_/g, " "))}</b> — ${esc(w.message || "")}</div>`).join("");
    main = `<div class="rev-bar"><span class="ref">${esc(ref)} · ${esc(cur.status)}</span>
        <div class="rev-nav"><button class="rev-flag ${flagged ? "on" : ""}" onclick="bfReviewFlag()">⚑ ${flagged ? "Flagged" : "Flag"}</button>
          <button class="rev-btn" onclick="bfReviewNav(-1)">←</button><button class="rev-btn" onclick="bfReviewNav(1)">→</button></div></div>
      <div class="rev-cols scr">
        <div class="rev-col"><div class="cl">Source · ${esc(doc.source_language || "auto")}</div><div class="rev-text">${esc(cur.source_text)}</div></div>
        <div class="rev-col tgt"><div class="cl">Translation · ${esc(doc.target_language)}</div><div class="rev-text">${esc(cur.target_text || "—")}</div>${notes}</div>
      </div>`;
  }
  $("#stage").innerHTML = `<div class="review">
    <div class="rev-list"><div class="lh"><div class="t">${esc(shorten(title, 32))}</div>
        <div class="m">${langs} · ${segs.length} segments · ${fmtCost(doc.totals && doc.totals.estimated_cost_usd)}</div>
        <div class="rev-filters">${filters.map(([f, l]) => `<div class="rev-filter ${R.filter === f ? "on" : ""}" onclick="bfReviewFilter('${f}')">${esc(l)}</div>`).join("")}</div></div>
      <div class="rev-rows scr">${rows}</div></div>
    <div class="rev-main">${main}</div></div>`;
}
async function renderValidation(stage) {
  const id = App.selected;
  if (!id) { placeholder(stage, "Validation", "EPUBCheck and structural validators."); return; }
  App.validation = App.validation || {};
  if (App.validation[id]) { drawValidation(App.validation[id]); } else { runValidation(); }
}
async function runValidation() {
  const id = App.selected;
  $("#stage").innerHTML = `<div class="wrap"><div class="empty">Running validators…</div></div>`;
  try {
    const r = await fetch("/api/jobs/" + encodeURIComponent(id) + "/validate", { method: "POST", headers: { [CSRF_HEADER]: CSRF_TOKEN } });
    const j = await r.json();
    if (!r.ok) { $("#stage").innerHTML = `<div class="wrap"><div class="empty">${esc(j.error || "Validation could not run.")}</div></div>`; return; }
    (App.validation = App.validation || {})[id] = j;
    if (App.screen === "validation" && App.selected === id) drawValidation(j);
  } catch (e) { $("#stage").innerHTML = `<div class="wrap"><div class="empty">Could not run validation.</div></div>`; }
}
function bfRevalidate() { runValidation(); }
function sevRank(s) { return s === "fatal" || s === "error" ? "bad" : s === "warning" ? "warn" : s === "info" ? "info" : "good"; }
function sevGlyph(cls) { return cls === "bad" ? "✗" : cls === "warn" ? "!" : cls === "info" ? "i" : "✓"; }
function drawValidation(rep) {
  const ec = rep.epubcheck || {}, bf = rep.bookforge_validators || {};
  const msgs = [...(bf.messages || []).map(m => ({ ...m, src: "BookForge" })), ...(ec.messages || []).map(m => ({ ...m, src: "EPUBCheck" }))];
  const errors = msgs.filter(m => m.severity === "error" || m.severity === "fatal").length;
  const warnings = msgs.filter(m => m.severity === "warning").length;
  const overall = errors ? "bad" : warnings ? "warn" : "good";
  const title = errors ? "Validation failed" : warnings ? `Passed with ${warnings} warning${warnings === 1 ? "" : "s"}` : "Passed";
  const ecUnavailable = ec.status === "unavailable";
  const sub = ec.ran ? `EPUBCheck ${esc(ec.version || "")} · ${esc(ec.status)}. BookForge validators: ${esc(bf.status || "-")}.`
    : `EPUBCheck not run. BookForge validators: ${esc(bf.status || "-")}.`;
  const note = ecUnavailable ? `<div class="val-note">EPUBCheck is unavailable — install <b>epubcheck</b> on PATH or set <b>BOOKFORGE_EPUBCHECK</b> to include the reader-compatibility pass. BookForge's own structural validators still ran.</div>` : "";
  const rows = msgs.length ? msgs.map(m => {
    const cls = sevRank(m.severity);
    return `<div class="val-item"><span class="val-dot ${cls}">${sevGlyph(cls)}</span>
      <div class="m"><div class="mt">${esc(m.text || m.code || "message")}</div>
        <div class="ml">${esc(m.src)} · ${esc(m.code || "")}${m.location ? " · " + esc(m.location) : ""}</div></div></div>`;
  }).join("") : `<div class="val-item"><span class="val-dot good">✓</span><div class="m"><div class="mt">No issues reported.</div></div></div>`;
  $("#stage").innerHTML = `<div class="wrap">
    <div class="val-hero"><div class="val-icon ${overall}">${overall === "bad" ? "✗" : overall === "warn" ? "!" : "✓"}</div>
      <div class="h"><div class="t">${title}</div><div class="s">${sub}</div></div>
      <button class="btn btn-ghost" onclick="bfRevalidate()">Re-run check</button></div>
    ${note}
    <div class="val-stats">
      <div class="val-stat"><div class="v ${errors ? "bad" : "good"}">${errors}</div><div class="l">Errors</div></div>
      <div class="val-stat"><div class="v ${warnings ? "warn" : "good"}">${warnings}</div><div class="l">Warnings</div></div>
      <div class="val-stat"><div class="v">${bf.xml_valid ? "OK" : "—"}</div><div class="l">Structure${bf.files_checked != null ? " · " + bf.files_checked + " files" : ""}</div></div>
    </div>
    <p class="sectlabel">Validator messages</p>
    <div class="val-list">${rows}</div></div>`;
}
const GL_CATEGORIES = ["person","place","object","invented","style","phrase","other"];
function renderGlossary(stage) {
  if (!App.glossary) {
    const langs = App.options.languages || [];
    const to = langs.includes("Italian") ? "Italian" : (langs.find(l => l !== "English") || "Italian");
    App.glossary = { from: "English", to, terms: [] };
  }
  const g = App.glossary;
  const langOpts = (App.options.languages || []).map(l => `<option value="${esc(l)}">`).join("");
  stage.innerHTML = `<div class="wrap">
    <div class="pagehead"><div><h1>Glossary</h1><p>Lock names, places and recurring terms to a fixed translation. Applied across every chapter for consistency.</p></div></div>
    <div class="gl-langs">
      <div><div class="field-label">From</div><input class="inp" id="gl_from" list="gllangs" value="${esc(g.from)}"></div>
      <span style="align-self:end;padding-bottom:12px;color:var(--faint)">→</span>
      <div><div class="field-label">Into</div><input class="inp" id="gl_to" list="gllangs" value="${esc(g.to)}"></div>
      <button class="btn btn-ghost" style="align-self:end" onclick="bfGlossaryReload()">Show</button>
    </div>
    <datalist id="gllangs">${langOpts}</datalist>
    <div class="gl-add">
      <input class="inp" id="gl_src" placeholder="Source term (e.g. the Grange)">
      <span style="color:var(--faint)">→</span>
      <input class="inp" id="gl_tgt" placeholder="Translation (e.g. la Grange)">
      <select class="inp" id="gl_cat" style="max-width:140px">${GL_CATEGORIES.map(c => `<option value="${c}">${c}</option>`).join("")}</select>
      <button class="btn btn-primary" style="white-space:nowrap;padding:11px 18px" onclick="bfGlossaryAdd()">Add term</button>
    </div>
    <div class="gl-status" id="gl_status"></div>
    <div class="gl-table" id="gl_table"><div class="empty" style="margin-top:20px">Loading…</div></div></div>`;
  loadGlossary();
}
async function loadGlossary() {
  const g = App.glossary;
  const q = `?source=${encodeURIComponent(g.from)}&target=${encodeURIComponent(g.to)}&scope=global`;
  let terms = [];
  try { terms = await (await fetch("/api/glossary" + q)).json(); } catch (e) { terms = []; }
  g.terms = Array.isArray(terms) ? terms : [];
  drawGlossaryTable();
}
function drawGlossaryTable() {
  const g = App.glossary, box = $("#gl_table"); if (!box) return;
  if (!g.terms.length) { box.innerHTML = `<div class="empty" style="margin-top:20px">No terms for ${esc(g.from)} → ${esc(g.to)} yet.</div>`; return; }
  const rows = g.terms.map(t => `<div class="gl-row">
    <div class="gl-c s">${esc(t.source)}</div><div class="gl-c t">${esc(t.target)}</div>
    <div class="gl-c cat">${esc(t.category || "")}</div>
    <div class="gl-c x" title="Remove" onclick="bfGlossaryRemove(${Number(t.id)})">×</div></div>`).join("");
  box.innerHTML = `<div class="gl-head"><div>Source</div><div>Translation</div><div>Category</div><div></div></div>${rows}
    <div class="gl-foot">${g.terms.length} term${g.terms.length === 1 ? "" : "s"} · global scope · ${esc(g.from)} → ${esc(g.to)}</div>`;
}
function bfGlossaryReload() {
  const f = $("#gl_from"), t = $("#gl_to");
  if (f) App.glossary.from = f.value.trim() || "English";
  if (t) App.glossary.to = t.value.trim() || "Italian";
  loadGlossary();
}
async function bfGlossaryAdd() {
  const g = App.glossary, status = $("#gl_status");
  const source = $("#gl_src").value.trim(), target = $("#gl_tgt").value.trim(), category = $("#gl_cat").value;
  if (!source || !target) { status.textContent = "enter a source term and its translation"; return; }
  status.textContent = "saving…";
  try {
    const r = await fetch("/api/glossary", {
      method: "POST",
      headers: { [CSRF_HEADER]: CSRF_TOKEN, "content-type": "application/json" },
      body: JSON.stringify({ source, target, category, scope: "global", source_language: g.from, target_language: g.to, always_active: true }),
    });
    const j = await r.json();
    if (!r.ok) { status.textContent = j.error || "could not add term"; return; }
    status.textContent = ""; $("#gl_src").value = ""; $("#gl_tgt").value = "";
    loadGlossary();
  } catch (e) { status.textContent = "could not add term"; }
}
async function bfGlossaryRemove(id) {
  try { await fetch("/api/glossary/" + id, { method: "DELETE", headers: { [CSRF_HEADER]: CSRF_TOKEN } }); loadGlossary(); } catch (e) {}
}

/* ---------------- boot ---------------- */
async function loadOptions() {
  try { const r = await fetch("/api/options"); if (r.ok) App.options = await r.json(); } catch (e) {}
}
async function loadProviderStatus() {
  try { App.providerKeys = await (await fetch("/api/providers")).json(); } catch (e) { App.providerKeys = {}; }
}
async function boot() {
  applyTheme();
  await Promise.all([loadOptions(), loadProviderStatus()]);
  render();
  // Keep the library list live while it's on screen (statuses/progress advance).
  setInterval(() => { if (App.screen === "library") loadLibraryJobs(); }, 4000);
}
boot();
</script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn test_state(token: &str) -> AppState {
        AppState {
            refresh: Duration::from_millis(250),
            csrf_token: token.to_string(),
            keys: Arc::new(Mutex::new(HashMap::new())),
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

    #[tokio::test]
    async fn retry_endpoint_rejects_missing_dashboard_token() {
        use axum::{body::Body, http::Request};
        use tower::ServiceExt;

        let response = dashboard_router(test_state("token-123"))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/jobs/not-real/retry")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("route should respond");

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
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
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("route should respond");
        assert_eq!(remove.status(), StatusCode::FORBIDDEN);
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
}
