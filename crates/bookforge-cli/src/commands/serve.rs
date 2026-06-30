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
    collections::HashMap, convert::Infallible, net::SocketAddr, path::PathBuf, process::Command,
    time::Duration,
};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Multipart, Path as AxumPath, State},
    http::StatusCode,
    response::{
        Html, IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use bookforge_core::{RunState, now_ms};
use bookforge_store::{JobRecord, JobStore, JobSummary, RetryScope};
use serde::Serialize;
use serde_json::json;

use crate::eventlog::{EventLogTailer, events_path_for};

/// Where browser-launched uploads and their outputs are written.
const UPLOAD_DIR: &str = ".bookforge/serve-uploads";

/// Cap on a multipart upload body (EPUBs in the regression corpus reach ~11 MB).
const MAX_UPLOAD_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, clap::Args)]
pub struct ServeArgs {
    /// Address to bind. Defaults to localhost only — the book text is served
    /// unauthenticated, so only widen this behind a tunnel you trust.
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
}

pub async fn run(args: ServeArgs) -> Result<()> {
    let addr: SocketAddr = args
        .bind
        .parse()
        .with_context(|| format!("invalid --bind address '{}'", args.bind))?;

    let state = AppState {
        refresh: Duration::from_millis(args.refresh_ms.clamp(50, 5_000)),
    };

    let app = Router::new()
        .route("/", get(index))
        .route("/api/jobs", get(list_jobs))
        .route("/api/jobs/{id}", get(job_detail))
        .route("/api/jobs/{id}/events", get(job_events))
        .route("/api/jobs/{id}/retry", post(retry_job))
        .route("/api/translate", post(launch_translate))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;
    let local = listener.local_addr().unwrap_or(addr);
    let url = format!("http://{local}/");

    println!("BookForge dashboard listening on {url}");
    if !local.ip().is_loopback() {
        println!(
            "  warning: bound to a non-loopback address — the book text is served \
             unauthenticated. Prefer an SSH tunnel over exposing this directly."
        );
    }
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

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn index() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
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

async fn retry_job(AxumPath(id): AxumPath<String>) -> Result<Json<serde_json::Value>, AppError> {
    let retried = tokio::task::spawn_blocking(move || -> Result<usize> {
        let store = JobStore::open_default()?;
        Ok(store.retry_segments(&id, RetryScope::All)?)
    })
    .await??;
    Ok(Json(json!({ "retried": retried })))
}

/// Launch a new translation from an uploaded EPUB.
///
/// Runs the translation as a detached `bookforge translate` subprocess. The
/// child inherits this process's environment, so provider API keys come from the
/// same env vars the CLI uses (`OPENROUTER_API_KEY`, etc.) — the browser never
/// handles secrets. The job is matched back to the dashboard by its unique input
/// path (returned to the client), since the run generates its own job id.
async fn launch_translate(mut multipart: Multipart) -> Result<Response, AppError> {
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

    let stem = sanitize_component(strip_epub_suffix(&file_name));
    let tag = format!("{}-{stem}", now_ms());
    let upload_dir = PathBuf::from(UPLOAD_DIR);
    std::fs::create_dir_all(&upload_dir)?;
    let input_path = upload_dir.join(format!("{tag}.epub"));
    std::fs::write(&input_path, &bytes)?;
    let out_path = upload_dir.join(format!("{tag}.{}.epub", sanitize_component(&target)));

    let provider = field_value(&fields, "provider").unwrap_or_else(|| "mock".to_string());
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
<title>BookForge dashboard</title>
<style>
:root {
  color-scheme: dark;
  --bg: #0f1419;
  --panel: #171c24;
  --panel-2: #1d2530;
  --text: #e6edf3;
  --muted: #8b97a6;
  --line: #2a3441;
  --accent: #2dd4bf;
  --accent-dim: #155e57;
  --warn: #f59e0b;
  --bad: #f87171;
  --good: #34d399;
}
* { box-sizing: border-box; }
body {
  margin: 0; background: var(--bg); color: var(--text);
  font: 14px/1.5 ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto, sans-serif;
}
header {
  display: flex; align-items: baseline; gap: 12px;
  padding: 14px 20px; border-bottom: 1px solid var(--line); background: var(--panel);
}
header h1 { font-size: 16px; margin: 0; letter-spacing: .3px; }
header .sub { color: var(--muted); font-size: 12px; }
header .live { margin-left: auto; color: var(--muted); font-size: 12px; display: flex; align-items: center; gap: 6px; }
.dot { width: 8px; height: 8px; border-radius: 50%; background: var(--muted); }
.dot.on { background: var(--good); box-shadow: 0 0 8px var(--good); }
main { display: grid; grid-template-columns: 320px 1fr; height: calc(100vh - 53px); }
#sidebar { border-right: 1px solid var(--line); overflow-y: auto; background: var(--panel); }
#sidebar .head { padding: 10px 14px; color: var(--muted); font-size: 11px; text-transform: uppercase; letter-spacing: .8px; }
.job { padding: 10px 14px; border-bottom: 1px solid var(--line); cursor: pointer; }
.job:hover { background: var(--panel-2); }
.job.sel { background: var(--panel-2); border-left: 3px solid var(--accent); padding-left: 11px; }
.job .id { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 12px; }
.job .meta { color: var(--muted); font-size: 11px; margin-top: 2px; display: flex; justify-content: space-between; gap: 8px; }
.minibar { height: 4px; background: var(--panel-2); border-radius: 2px; margin-top: 6px; overflow: hidden; }
.minibar > i { display: block; height: 100%; background: var(--accent); }
.badge { display: inline-block; padding: 1px 7px; border-radius: 999px; font-size: 11px; border: 1px solid var(--line); }
.badge.running { color: var(--accent); border-color: var(--accent-dim); }
.badge.done, .badge.completed { color: var(--good); border-color: #1f5f4a; }
.badge.failed, .badge.error { color: var(--bad); border-color: #6e2a2a; }
#detail { overflow-y: auto; padding: 22px 26px; }
.empty { color: var(--muted); margin-top: 40px; text-align: center; }
.title { display: flex; align-items: center; gap: 12px; margin-bottom: 4px; }
.title h2 { font-size: 18px; margin: 0; font-family: ui-monospace, Menlo, monospace; }
.paths { color: var(--muted); font-size: 12px; margin-bottom: 18px; }
.paths code { color: var(--text); }
.bar { height: 22px; background: var(--panel-2); border-radius: 6px; overflow: hidden; position: relative; border: 1px solid var(--line); }
.bar > i { display: block; height: 100%; background: linear-gradient(90deg, var(--accent-dim), var(--accent)); transition: width .3s ease; }
.bar > span { position: absolute; inset: 0; display: flex; align-items: center; justify-content: center; font-size: 12px; font-variant-numeric: tabular-nums; }
.grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(130px, 1fr)); gap: 10px; margin: 20px 0; }
.stat { background: var(--panel); border: 1px solid var(--line); border-radius: 8px; padding: 12px 14px; }
.stat .k { color: var(--muted); font-size: 11px; text-transform: uppercase; letter-spacing: .6px; }
.stat .v { font-size: 20px; font-variant-numeric: tabular-nums; margin-top: 4px; }
.stat .v.good { color: var(--good); } .stat .v.warn { color: var(--warn); } .stat .v.bad { color: var(--bad); }
.row { display: flex; gap: 18px; flex-wrap: wrap; align-items: center; margin-bottom: 8px; }
button.retry {
  background: var(--accent-dim); color: var(--text); border: 1px solid var(--accent);
  border-radius: 6px; padding: 7px 14px; cursor: pointer; font-size: 13px;
}
button.retry:hover { background: var(--accent); color: #06231f; }
button.retry:disabled { opacity: .5; cursor: default; }
.toast { color: var(--muted); font-size: 12px; }
.panel { background: var(--panel); border: 1px solid var(--line); border-radius: 8px; margin-top: 18px; }
.panel h3 { font-size: 12px; text-transform: uppercase; letter-spacing: .6px; color: var(--muted); margin: 0; padding: 10px 14px; border-bottom: 1px solid var(--line); }
.panel .body { max-height: 260px; overflow-y: auto; padding: 6px 0; }
.line { padding: 3px 14px; font-family: ui-monospace, Menlo, monospace; font-size: 12px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.line.warn { color: var(--warn); } .line.bad { color: var(--bad); }
.line .t { color: var(--muted); margin-right: 8px; }
.newrun { border-bottom: 1px solid var(--line); }
.newrun > summary { padding: 10px 14px; cursor: pointer; color: var(--accent); font-size: 13px; user-select: none; list-style: none; }
.newrun > summary::-webkit-details-marker { display: none; }
.newrun > summary:hover { background: var(--panel-2); }
#launchform { display: flex; flex-direction: column; gap: 8px; padding: 2px 14px 14px; }
#launchform label { display: flex; flex-direction: column; gap: 3px; font-size: 11px; color: var(--muted); }
#launchform input, #launchform select { background: var(--panel-2); color: var(--text); border: 1px solid var(--line); border-radius: 5px; padding: 6px 8px; font-size: 13px; }
#launchform input[type=file] { padding: 5px; }
#launchform button { margin-top: 4px; }
#launchform .hint { font-size: 10px; color: var(--muted); margin: 2px 0 0; line-height: 1.4; }
#launchstatus { font-size: 11px; min-height: 14px; }
</style>
</head>
<body>
<header>
  <h1>BookForge</h1>
  <span class="sub">local monitoring dashboard</span>
  <span class="live"><span class="dot" id="livedot"></span><span id="livetxt">idle</span></span>
</header>
<main>
  <div id="sidebar">
    <details id="newrun" class="newrun">
      <summary>＋ New translation</summary>
      <form id="launchform" onsubmit="return launch(event)">
        <label>EPUB file<input type="file" name="file" accept=".epub" required></label>
        <label>Target language<input type="text" name="target" placeholder="Italian" required></label>
        <label>Source language (optional)<input type="text" name="source" placeholder="auto-detect"></label>
        <label>Provider<select name="provider">
          <option value="mock">mock (offline test)</option>
          <option value="deepseek">deepseek</option>
          <option value="openrouter">openrouter</option>
          <option value="openai-compatible">openai-compatible</option>
        </select></label>
        <label>Model (optional)<input type="text" name="model" placeholder="e.g. deepseek/deepseek-v4-flash"></label>
        <label>Profile<select name="profile">
          <option value="v1-fast">v1-fast</option>
          <option value="safe">safe</option>
          <option value="balanced">balanced</option>
          <option value="fastest">fastest</option>
          <option value="free-tier">free-tier</option>
          <option value="turbo-text-only">turbo-text-only</option>
        </select></label>
        <button type="submit" class="retry">Launch translation</button>
        <span class="toast" id="launchstatus"></span>
        <p class="hint">Provider API keys are read from the environment of the <code>bookforge serve</code> process (e.g. <code>OPENROUTER_API_KEY</code>).</p>
      </form>
    </details>
    <div class="head">Jobs</div><div id="jobs"></div>
  </div>
  <div id="detail"><div class="empty">Select a job to monitor.</div></div>
</main>
<script>
const $ = (sel, el) => (el || document).querySelector(sel);
let selected = null;
let es = null;

function shorten(s, n) { s = s || ""; return s.length > n ? s.slice(0, n - 1) + "…" : s; }
function num(n) { return (n || 0).toLocaleString(); }
function pct(done, total) { return total > 0 ? Math.min(100, Math.round(done / total * 100)) : 0; }

function elapsedSecs(s) {
  if (s.finished && s.finished_elapsed_ms != null) return s.finished_elapsed_ms / 1000;
  if (s.first_timestamp_ms != null && s.last_timestamp_ms != null && s.last_timestamp_ms >= s.first_timestamp_ms)
    return (s.last_timestamp_ms - s.first_timestamp_ms) / 1000;
  return 0;
}
function segPerMin(s) { const e = elapsedSecs(s); return e > 0 ? s.done_segments / e * 60 : 0; }
function etaSecs(s) { const r = Math.max(0, (s.total_segments || 0) - (s.done_segments || 0)); const pm = segPerMin(s); return pm > 0 ? r / (pm / 60) : 0; }
function fmtDur(secs) {
  secs = Math.round(secs);
  if (secs <= 0) return "—";
  const h = Math.floor(secs / 3600), m = Math.floor((secs % 3600) / 60), s = secs % 60;
  return h > 0 ? `${h}h${String(m).padStart(2,"0")}m` : (m > 0 ? `${m}m${String(s).padStart(2,"0")}s` : `${s}s`);
}
function badgeClass(status) { return (status || "").toLowerCase().replace(/[^a-z]/g, ""); }

async function loadJobs() {
  let jobs = [];
  try { jobs = await (await fetch("/api/jobs")).json(); } catch (e) { return; }
  const box = $("#jobs");
  box.innerHTML = "";
  if (!jobs.length) { box.innerHTML = '<div class="job"><span class="meta">No jobs yet. Run a translation.</span></div>'; return; }
  for (const j of jobs) {
    const el = document.createElement("div");
    el.className = "job" + (j.id === selected ? " sel" : "");
    const p = pct(j.done, j.total_segments);
    el.innerHTML =
      `<div class="id">${shorten(j.id, 30)}</div>
       <div class="meta"><span class="badge ${badgeClass(j.status)}">${j.status}</span><span>${j.done}/${j.total_segments}</span></div>
       <div class="minibar"><i style="width:${p}%"></i></div>
       <div class="meta"><span>${shorten(j.provider + " / " + j.model, 28)}</span><span>${j.target_lang}</span></div>`;
    el.onclick = () => selectJob(j.id);
    box.appendChild(el);
  }
}

async function selectJob(id) {
  selected = id;
  for (const el of document.querySelectorAll(".job")) el.classList.remove("sel");
  let detail;
  try { const r = await fetch("/api/jobs/" + encodeURIComponent(id)); if (!r.ok) throw new Error(); detail = await r.json(); }
  catch (e) { $("#detail").innerHTML = '<div class="empty">Could not load job.</div>'; return; }
  renderDetail(detail);
  await loadJobs();
  openStream(id);
}

function renderDetail(d) {
  const s = d.state || {};
  $("#detail").innerHTML = `
    <div class="title"><h2>${d.id}</h2><span class="badge ${badgeClass(d.status)}">${d.status}</span></div>
    <div class="paths"><code>${shorten(d.input_path, 60)}</code> &rarr; <code>${shorten(d.output_path, 60)}</code>
      &nbsp;&middot;&nbsp; ${d.provider} / ${d.model} &nbsp;&middot;&nbsp; ${d.source_lang || "auto"} &rarr; ${d.target_lang}</div>
    <div class="bar"><i id="barfill"></i><span id="bartext"></span></div>
    <div class="grid" id="stats"></div>
    <div class="row">
      <button class="retry" id="retrybtn">Retry failed / needs-review</button>
      <span class="toast" id="toast"></span>
    </div>
    <div class="panel"><h3>Issues</h3><div class="body" id="issues"></div></div>
    <div class="panel"><h3>Recent events</h3><div class="body" id="events"></div></div>`;
  $("#retrybtn").onclick = () => retry(d.id);
  updateState(s);
}

function updateState(s) {
  const total = s.total_segments || 0, done = s.done_segments || 0;
  const p = pct(done, total);
  const fill = $("#barfill"), txt = $("#bartext");
  if (fill) fill.style.width = p + "%";
  if (txt) txt.textContent = `${done} / ${total}  (${p}%)`;

  const stats = [
    ["done", done, ""], ["succeeded", s.succeeded || 0, "good"], ["cached", s.cached || 0, ""],
    ["needs review", s.needs_review || 0, (s.needs_review ? "warn" : "")], ["failed", s.failed || 0, (s.failed ? "bad" : "")],
    ["active req", `${s.active_requests || 0}/${s.target_concurrency || 0}`, ""],
    ["seg/min", segPerMin(s).toFixed(1), ""], ["elapsed", fmtDur(elapsedSecs(s)), ""], ["eta", s.finished ? "done" : fmtDur(etaSecs(s)), ""],
    ["tokens in", num(s.input_tokens), ""], ["tokens out", num(s.output_tokens), ""],
  ];
  const box = $("#stats");
  if (box) box.innerHTML = stats.map(([k, v, cls]) => `<div class="stat"><div class="k">${k}</div><div class="v ${cls}">${v}</div></div>`).join("");

  const ibox = $("#issues");
  if (ibox) {
    const issues = s.recent_issues || [];
    ibox.innerHTML = issues.length
      ? issues.slice().reverse().map(i => `<div class="line ${i.level === "Error" ? "bad" : "warn"}">${i.level === "Error" ? "✗" : "⚠"} ${i.kind}: ${shorten(i.message, 100)}</div>`).join("")
      : '<div class="line"><span class="t">none</span></div>';
  }
  const ebox = $("#events");
  if (ebox) {
    const evs = s.recent_events || [];
    ebox.innerHTML = evs.length
      ? evs.slice().reverse().map(fmtEvent).join("")
      : '<div class="line"><span class="t">waiting…</span></div>';
  }
}

function fmtEvent(ev) {
  const key = Object.keys(ev)[0];
  const v = ev[key] || {};
  let cls = "", body = key;
  switch (key) {
    case "SegmentFinished": body = `segment ${shorten(v.segment_id, 18)} → ${v.status}`; if (v.status === "failed") cls = "bad"; else if (v.status === "needs_review") cls = "warn"; break;
    case "SegmentStarted": body = `segment ${shorten(v.segment_id, 18)} started`; break;
    case "RequestStarted": body = `request started (${v.active_requests}/${v.target_concurrency})`; break;
    case "RequestFinished": body = `request ${v.status} · ${v.latency_ms}ms`; if (v.status !== "ok" && v.status !== "succeeded") cls = "warn"; break;
    case "StageStarted": body = `stage: ${v.stage}`; break;
    case "StageFinished": body = `stage complete: ${v.stage}`; break;
    case "SegmentationFinished": body = `segmented into ${v.segment_count} segments`; break;
    case "CacheScanFinished": body = `cache scan: ${v.hits} hits / ${v.misses} misses`; break;
    case "CheckpointFlushed": body = `checkpoint flushed (${v.flushed_count})`; break;
    case "ConcurrencyChanged": body = `concurrency ${v.previous} → ${v.current} (${v.reason})`; break;
    case "Warning": body = `⚠ ${v.kind}: ${shorten(v.message, 90)}`; cls = "warn"; break;
    case "Error": body = `✗ ${v.kind}: ${shorten(v.message, 90)}`; cls = "bad"; break;
    case "TranslationFinished": body = `finished: ${v.succeeded} ok, ${v.cached} cached, ${v.needs_review} review, ${v.failed} failed`; cls = "good"; break;
  }
  return `<div class="line ${cls}">${body}</div>`;
}

function setLive(on, txt) {
  $("#livedot").classList.toggle("on", on);
  $("#livetxt").textContent = txt;
}

function openStream(id) {
  if (es) { es.close(); es = null; }
  es = new EventSource("/api/jobs/" + encodeURIComponent(id) + "/events");
  setLive(true, "live");
  es.addEventListener("state", (e) => { if (id === selected) { try { updateState(JSON.parse(e.data)); } catch (_) {} } });
  es.addEventListener("done", () => { setLive(false, "finished"); if (es) { es.close(); es = null; } loadJobs(); });
  es.onerror = () => { setLive(false, "reconnecting…"); };
}

async function retry(id) {
  const btn = $("#retrybtn"), toast = $("#toast");
  btn.disabled = true; toast.textContent = "submitting…";
  try {
    const r = await fetch("/api/jobs/" + encodeURIComponent(id) + "/retry", { method: "POST" });
    const j = await r.json();
    toast.textContent = r.ok ? `marked ${j.retried} segment(s) — run: bookforge resume ${id}` : (j.error || "retry failed");
  } catch (e) { toast.textContent = "retry failed"; }
  btn.disabled = false;
  loadJobs();
}

let pendingInput = null;
async function launch(ev) {
  ev.preventDefault();
  const form = ev.target;
  const status = $("#launchstatus");
  status.textContent = "uploading…";
  try {
    const r = await fetch("/api/translate", { method: "POST", body: new FormData(form) });
    const j = await r.json();
    if (!r.ok) { status.textContent = j.error || "launch failed"; return false; }
    status.textContent = "started — locating job…";
    pendingInput = j.input_path;
    form.reset();
    $("#newrun").open = false;
    await loadJobs();
    setTimeout(() => trySelectPending(0), 800);
  } catch (e) { status.textContent = "launch failed"; }
  return false;
}

async function trySelectPending(attempt) {
  if (!pendingInput || attempt > 25) { pendingInput = null; return; }
  let jobs = [];
  try { jobs = await (await fetch("/api/jobs")).json(); } catch (e) {}
  const match = jobs.find(j => j.input_path === pendingInput);
  if (match) {
    pendingInput = null;
    $("#launchstatus").textContent = "";
    await loadJobs();
    selectJob(match.id);
  } else {
    setTimeout(() => trySelectPending(attempt + 1), 900);
  }
}

loadJobs();
setInterval(loadJobs, 4000);
</script>
</body>
</html>
"##;
