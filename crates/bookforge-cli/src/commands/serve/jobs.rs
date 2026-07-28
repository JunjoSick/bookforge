use super::*;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
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
    let lease_stale_after = state.runtime_lease_stale_after;
    let outcome = tokio::task::spawn_blocking(move || {
        runtime_settings_view(&store_path, &id, lease_stale_after)
    })
    .await?;
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
    let lease_stale_after = state.runtime_lease_stale_after;
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
        let mut view = runtime_settings_view(&store_path, &id, lease_stale_after)?
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
    // Held across the whole read-modify-write so a second tab cannot stage a
    // rebuilt EPUB from a snapshot taken before this correction lands.
    let lock = job_correction_lock(&state, &id)?;
    let _guard = lock.lock().await;
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
    request: Option<Json<ResumeJobRequest>>,
) -> Result<Response, AppError> {
    if let Some(response) = reject_mutation(&headers, &state) {
        return Ok(response);
    }
    let request = request.map(|Json(request)| request).unwrap_or_default();
    let store_path = state.store_path.clone();
    let lease_stale_after = state.runtime_lease_stale_after;
    let lookup = id.clone();
    let action = tokio::task::spawn_blocking(move || -> Result<Option<ResumeJobAction>> {
        let store = JobStore::open(&store_path)?;
        let Some(job) = store.get_job(&lookup)? else {
            return Ok(None);
        };
        let snapshot = store.load_job_config_snapshot(&lookup)?;
        let live = matches!(
            crate::control::runtime_lease_state(&lookup, lease_stale_after),
            crate::control::RuntimeLeaseState::Fresh(_)
        );
        let resumable = !store.resumable_segment_ids(&lookup)?.is_empty()
            || (job_status_has_unfinished_pipeline_work(&job.status) && snapshot.is_some());
        let force = !live && job.status == "paused";
        Ok(Some(ResumeJobAction {
            live,
            resumable,
            force,
            provider: job.provider,
            api_key_env: snapshot.and_then(|snapshot| snapshot.api_key_env),
        }))
    })
    .await??;
    let Some(action) = action else {
        return Ok((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no such job" })),
        )
            .into_response());
    };
    if action.live {
        let path = crate::control::request_job_control(&id, ControlCommand::Resume)?;
        return Ok(Json(json!({
            "command": "resume",
            "mode": "signaled",
            "control_path": path,
        }))
        .into_response());
    }
    if !action.resumable {
        return Ok(bad_request(
            "the worker is not alive and this job has no resumable work",
        ));
    }

    let api_key_env = if action.provider == "mock" {
        None
    } else {
        action
            .api_key_env
            .or_else(|| provider_key_env(&action.provider).map(str::to_string))
    };
    let key = resolve_dashboard_provider_key(
        &state,
        &action.provider,
        request.api_key,
        api_key_env.as_deref(),
    )?;
    if action.provider != "mock" && key.is_none() {
        return Ok(missing_resume_key(&action.provider, api_key_env.as_deref()));
    }

    let Some(mut launch_claim) = crate::control::RuntimeLaunchClaim::acquire(&id)? else {
        return Ok(Json(json!({
            "command": "resume",
            "mode": "launching",
        }))
        .into_response());
    };
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
    if action.force {
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
    configure_dashboard_child_environment(&mut command, api_key_env.as_deref().zip(key.as_deref()));
    #[cfg(test)]
    if let Some(launches) = &state.resume_launches {
        launches.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Some(environments) = &state.resume_child_environments {
            let environment = command
                .as_std()
                .get_envs()
                .map(|(name, value)| (name.to_os_string(), value.map(ToOwned::to_owned)))
                .collect();
            environments
                .lock()
                .map_err(|_| anyhow::anyhow!("resume environment recorder is unavailable"))?
                .push(environment);
        }
        launch_claim.persist_until_worker();
        return Ok(Json(json!({
            "command": "resume",
            "mode": "spawned",
            "pid": 0,
            "forced": action.force,
        }))
        .into_response());
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
        "forced": action.force,
    }))
    .into_response())
}

#[derive(Default, Deserialize)]
struct ResumeJobRequest {
    api_key: Option<String>,
}

struct ResumeJobAction {
    live: bool,
    resumable: bool,
    force: bool,
    provider: String,
    api_key_env: Option<String>,
}

fn missing_resume_key(provider: &str, api_key_env: Option<&str>) -> Response {
    let env_note = api_key_env
        .map(|env| format!(" ({env})"))
        .unwrap_or_default();
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "error": format!(
                "API key for provider '{provider}'{env_note} is unavailable; supply it to resume this job"
            ),
            "requires_api_key": true,
            "provider": provider,
            "api_key_env": api_key_env,
        })),
    )
        .into_response()
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
    let lease_stale_after = state.runtime_lease_stale_after;
    let outcome = tokio::task::spawn_blocking(move || -> Result<Option<String>> {
        let store = JobStore::open(store_path)?;
        if store.get_job(&id)?.is_none() {
            return Ok(None);
        }
        if !matches!(
            crate::control::runtime_lease_state(&id, lease_stale_after),
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

/// Lock serializing manual corrections for one job.
///
/// Applying a correction is a read-modify-write across the whole book, so two
/// concurrent requests can both read the same pre-correction snapshot and the
/// later rename publishes an EPUB missing the earlier edit. The registry is
/// keyed by job id, so corrections to different books still run in parallel.
pub(super) fn job_correction_lock(
    state: &AppState,
    job_id: &str,
) -> Result<Arc<tokio::sync::Mutex<()>>> {
    let mut locks = state
        .correction_locks
        .lock()
        .map_err(|_| anyhow::anyhow!("correction lock registry is unavailable"))?;
    Ok(locks.entry(job_id.to_string()).or_default().clone())
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
    lease_stale_after: Duration,
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
    let lease_state = crate::control::runtime_lease_state(id, lease_stale_after);
    let (lease, live, applied_revision) = match lease_state {
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
