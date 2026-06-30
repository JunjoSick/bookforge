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

use std::{convert::Infallible, net::SocketAddr, path::PathBuf, process::Command, time::Duration};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{Path as AxumPath, State},
    http::StatusCode,
    response::{
        Html, IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use bookforge_core::RunState;
use bookforge_store::{JobRecord, JobStore, JobSummary, RetryScope};
use serde::Serialize;
use serde_json::json;

use crate::eventlog::{EventLogTailer, events_path_for};

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
</style>
</head>
<body>
<header>
  <h1>BookForge</h1>
  <span class="sub">local monitoring dashboard</span>
  <span class="live"><span class="dot" id="livedot"></span><span id="livetxt">idle</span></span>
</header>
<main>
  <div id="sidebar"><div class="head">Jobs</div><div id="jobs"></div></div>
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

loadJobs();
setInterval(loadJobs, 4000);
</script>
</body>
</html>
"##;
