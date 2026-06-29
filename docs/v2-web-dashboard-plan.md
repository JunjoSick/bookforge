# v2 Non-Developer UI — Local Web Dashboard (deferred)

> **Status:** planned, not built. The *terminal* half of v2 (shared `RunState`
> view layer + ratatui TUI + `watch`) ships on branch **`feat/tui-monitoring`**
> (PR against `main`). This document is the plan for the remaining half — the
> browser UI for non-developers — which gates the actual **v2** release/tag.
> Written 2026-06-29.

## Context / why

v1.x is CLI-only. We decided v2's UI focus is **monitoring & controlling runs**,
serving **both** terminal power users *and* non-developers (translator friends
to dogfood). The chosen architecture was: build a renderer-agnostic view layer
first, then a ratatui TUI, then a local web dashboard — all on the same layer.

The first two are done and being merged. The web dashboard is what makes v2
reachable by people who won't open a terminal. **Do not tag v2 until this is
ready** (per owner). The groundwork is already in place:

- `bookforge_core::RunState` — folds `ProgressEvent`s into displayable state,
  `#[derive(Serialize, Deserialize)]`, timing derived from event timestamps so a
  finished log replays identically to live. **The web layer ships it over the
  wire essentially for free.**
- `bookforge_core::ProgressEvent` — also `Serialize + Deserialize`; this is the
  JSONL wire format persisted at `.bookforge/runs/<job>/events.jsonl`.
- `bookforge_store::JobStore` — `list_job_summaries()`, `get_job`, `summary`,
  `retry_segments` (all already added in the TUI work).
- Event-log tailing logic exists in `crates/bookforge-cli/src/commands/watch.rs`
  (`pump_file`, byte-offset follow tolerant of partial lines) and the events-path
  resolution — **extract these into a shared module for reuse** (see below).
- `crates/bookforge-cli/src/commands/review.rs` already emits a static HTML page
  from inline Rust string consts (`REVIEW_CSS`, `REVIEW_HTML_*`) — **copy that
  asset pattern** so there is no SPA/node build pipeline and `cargo-dist`
  packaging stays unchanged.

## Goal

A `bookforge serve` command that runs a small **local web server** exposing job
monitoring (and, as a fast-follow, launching) in a browser — reachable by
non-devs and usable remotely (run translation on a box, watch from a laptop).

## Architecture

- **Embedded `axum` server**, behind a new **`serve` cargo feature** that is
  **opt-in initially** (axum/tower pull a fair amount; keep the default binary
  lean — `tui` is default-on, `serve` starts optional, flip to default for the
  release if desired). Gate the command exactly like `watch` is gated for `tui`.
- **SSE (server-sent events)** for live updates — one-way server→client is a
  perfect fit for monitoring and needs no extra deps beyond axum. (Not WebSocket.)
- **Server-side fold:** the server tails `events.jsonl`, folds into `RunState`,
  and pushes the `RunState` snapshot as JSON on each change. Simplest correct
  design; `RunState` is small (bounded event/issue buffers). Avoids duplicating
  fold logic in JS.
- **Frontend = inline string consts** (`DASHBOARD_HTML/CSS/JS`), vanilla JS, no
  build step — mirrors `review.rs`.
- **Bind `127.0.0.1` by default.** The book text is private (same warning the
  review page shows).

### New subcommand

`bookforge serve [--bind 127.0.0.1:PORT] [--open]` — `--open` launches the
browser (`xdg-open`/`open`, or the `webbrowser` crate).

### Endpoints

- `GET /` → dashboard HTML (job list + detail shell).
- `GET /api/jobs` → JSON from `store.list_job_summaries()`.
- `GET /api/jobs/:id` → `JobRecord` + `JobSummary` + `RunState::from_events(<log>)`.
- `GET /api/jobs/:id/events` (**SSE**) → tail the log, fold, push `RunState`
  snapshots as `data:` JSON on change. Reuse the shared tailer.
- `POST /api/jobs/:id/retry` → `store.retry_segments(id, RetryScope::All)`,
  returns the count (control parity with the TUI `r` key).
- (Optional) mount/link the existing per-job review `index.html` for finished jobs.

## Reuse (lean on what exists)

- `RunState` + `RunState::from_events` (core) — all aggregation.
- `ProgressEvent` serde (core) — wire format.
- `JobStore::{list_job_summaries,get_job,summary,retry_segments}` (store).
- **Refactor:** extract `watch.rs::pump_file` + events-path resolution into a new
  `crates/bookforge-cli/src/eventlog.rs`; have both `watch` and `serve` use it
  (and optionally `tail`/`status`). Do this refactor first so the tailer has one
  home.
- `review.rs` inline-asset pattern for the dashboard page.

## Non-developer ergonomics (the actual point)

- `--open` to drop them straight into the browser.
- **Launch-from-browser (fast-follow, likely v2.1):** a "New translation" form —
  upload EPUB + target language + provider/profile dropdown → `POST /api/translate`
  spawns a background run (reusing the translate engine) and streams its events to
  the dashboard. This is the headline non-dev feature, but it introduces a real
  surface: **file upload, provider config, and API-key handling in a browser**
  (where do secrets come from? auth?). **Recommendation: v2.0 ships monitor +
  retry only** (launching still via CLI); add launch-from-browser once the
  monitoring plumbing is proven.
- **Security for remote dogfooding:** localhost by default. For friends watching
  remotely, prefer an SSH tunnel, or a simple shared token (`--token` + query
  param) if binding beyond localhost. **Never expose unauthenticated on
  `0.0.0.0` by default.**

## Risks / decisions (already reasoned through)

- SSE over WebSocket — simpler, sufficient for one-way monitoring.
- Server folds and pushes `RunState` snapshots (vs. shipping raw events for the
  client to fold) — simplest; bandwidth is fine at book-translation event rates.
- `serve` feature opt-in first; consider default-on for the release.
- Launch-from-browser deferred behind auth/upload/secrets considerations.

## Files to add / modify

- `crates/bookforge-cli/Cargo.toml` — add `axum` (optional) + `serve` feature.
- `crates/bookforge-cli/src/commands/serve.rs` (new) — server, endpoints, inline
  HTML/CSS/JS consts.
- `crates/bookforge-cli/src/eventlog.rs` (new) — shared tailer + path resolution,
  extracted from `watch.rs`.
- `crates/bookforge-cli/src/{main.rs,commands/mod.rs}` — register `serve` (gated).
- `bookforge-store` — already has `list_job_summaries`; no change expected.

## Verification

- **Unit:** `/api/jobs/:id` returns a `RunState` whose totals match
  `bookforge status` for a mock job; eventlog tailer unit test (partial-line
  tolerance, append follow).
- **Manual (offline):** `bookforge translate … --provider mock`, then
  `bookforge serve --open` → job list renders, open detail, SSE updates as a
  (real-provider) run progresses; click retry → segments flip to `retry_pending`
  (confirm via `bookforge status`). Test from a second device over an SSH tunnel.
