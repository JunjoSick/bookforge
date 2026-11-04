use super::resolve_job_input;
use crate::progress::finalize_reporter;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use anyhow::Result;
use bookforge_core::{
    ControlCommand, ProgressEvent, ProgressSink, ResolvedRunSettings, ResolvedRunSettingsSnapshot,
    RunConfigSnapshot,
    config::{DoubleCheckMode, FallbackScope},
    merge_scope_terms,
    progress::now_ms,
    segment::{
        BlockTranslation, Segment, SegmentStatus, build_segments, compute_cache_namespace,
        compute_cache_namespace_v1,
    },
    select_glossary_for_segments,
};
use bookforge_epub::{read_epub, rebuild_epub_with_options};
use bookforge_llm::{
    ContextRegistry, ContextRunConfig, EntityRunConfig, GlossaryRunConfig, MockProvider,
    OpenAiCompatibleConfig, OpenAiCompatibleProvider, QaSegmentReview, SegmentTranslation,
    StyleRunConfig, TranslationRunConfig, run_double_check,
};
use bookforge_store::{JobRecord, JobStore, StoredBlockTranslation};
use clap::Args;

const PAUSED_RESUME_WAIT: Duration = Duration::from_secs(10);
const PAUSED_RESUME_POLL: Duration = Duration::from_millis(250);

use crate::{
    QaMode,
    commands::{reconfigure, validate},
    performance::performance_summary_from_events,
    report::{ReportInput, TranslationQaInput, persist_qa_reviews_best_effort, write_report},
};

use super::translate::{
    CacheContext, CheckpointRunContext, FallbackPassConfig, ProgressRequestProvider,
    apply_cached_translations, apply_double_check_corrections, job_was_stopped, mark_job_finished,
    mock_mode, persist_corrected_translations, print_stopped_resume_hint,
    qa_reviews_for_mode_with_max_output_tokens, run_checkpointed_translation, run_fallback_pass,
};

#[derive(Debug, Args)]
pub struct ResumeArgs {
    pub job_id: String,

    #[arg(long)]
    pub concurrency: Option<usize>,

    #[arg(long)]
    pub max_attempts: Option<usize>,

    #[arg(long)]
    pub provider_max_attempts: Option<usize>,

    #[arg(long)]
    pub validation_max_attempts: Option<usize>,

    #[arg(long, value_enum)]
    pub qa: Option<QaMode>,

    #[arg(long)]
    pub timeout_seconds: Option<u64>,

    #[arg(long)]
    pub max_output_tokens: Option<u32>,

    #[arg(long)]
    pub ui: Option<crate::progress::UiMode>,

    #[arg(long)]
    pub progress_jsonl: Option<PathBuf>,

    #[arg(long)]
    pub output: Option<PathBuf>,

    /// Ask DeepSeek-family endpoints to disable thinking mode (adds the
    /// provider-appropriate suppression parameter; ignored elsewhere).
    #[arg(long, default_value_t = false)]
    pub no_thinking: bool,

    // --- finalize-stage flags previously translate-only (CLI-16) -----------
    // Resume inherits the original run's settings from its snapshot; these
    // flags override that baseline so a resumed run can finalize exactly like
    // a fresh translate would. `Option` fields only apply when provided.
    /// Run BookForge validators and EPUBCheck on the rebuilt EPUB.
    #[arg(long, default_value_t = false)]
    pub validate_output: bool,

    /// Treat EPUBCheck warnings as errors. Implies --validate-output.
    #[arg(long, default_value_t = false)]
    pub strict_epubcheck: bool,

    #[arg(long)]
    pub qa_concurrency: Option<usize>,

    #[arg(long)]
    pub qa_batch_target_tokens: Option<usize>,

    #[arg(
        long,
        value_parser = clap::value_parser!(u32).range(1..)
    )]
    pub qa_max_output_tokens: Option<u32>,

    #[arg(long)]
    pub qa_model: Option<String>,

    #[arg(long)]
    pub qa_provider: Option<String>,

    #[arg(long)]
    pub qa_base_url: Option<String>,

    #[arg(long)]
    pub qa_api_key_env: Option<String>,

    #[arg(long, value_enum)]
    pub double_check: Option<DoubleCheckMode>,

    #[arg(long)]
    pub double_check_model: Option<String>,

    #[arg(long)]
    pub double_check_provider: Option<String>,

    #[arg(long)]
    pub double_check_base_url: Option<String>,

    #[arg(long)]
    pub double_check_api_key_env: Option<String>,

    #[arg(long)]
    pub double_check_concurrency: Option<usize>,

    #[arg(long)]
    pub fallback_provider: Option<String>,

    #[arg(long)]
    pub fallback_model: Option<String>,

    #[arg(long)]
    pub fallback_base_url: Option<String>,

    #[arg(long)]
    pub fallback_api_key_env: Option<String>,

    #[arg(long, value_enum)]
    pub fallback_only: Option<FallbackScope>,

    #[arg(
        long,
        default_value_t = false,
        help = "Relaunch even when a worker appears active; only use if it is gone because this can double-run the job"
    )]
    pub force: bool,
}

pub async fn run(
    args: ResumeArgs,
    cancel_token: tokio_util::sync::CancellationToken,
) -> Result<()> {
    let store = JobStore::open_default()?;
    // Canonical open point: drain warn-on-open storage diagnostics so legacy
    // unknown statuses / skipped hardening are visible at resume start.
    for diagnostic in store.take_diagnostics() {
        tracing::warn!(surface = "resume", "{diagnostic}");
    }
    let Some(job) = store.get_job(&args.job_id)? else {
        anyhow::bail!("job '{}' was not found", args.job_id);
    };
    if job.status == "paused" && !args.force {
        return tokio::select! {
            _ = cancel_token.cancelled() => Ok(()),
            result = live_fast_resume_paused_job(&store, &args) => result,
        };
    }
    // Finding (lifecycle audit): validate the run configuration snapshot,
    // overrides, paths, and flags BEFORE acquiring the launch claim or
    // touching the job's status/control state. A failed validation leaves the
    // prior status and control file exactly as they were.
    let mut snapshot = load_resume_snapshot(&store, &args.job_id)?;
    let overrides = load_resume_overrides_for_status(&args.job_id, &job.status)?;
    validate_resume_snapshot(&args.job_id, &snapshot)?;
    let _validated_input = resolve_job_input(&job, &snapshot)?;
    let _validated_output = resume_output_path(&args, &snapshot)?;
    // Flag validation is cheap and IO-free; run it up front so a bad flag
    // combination (e.g. --fallback-provider without --fallback-model) never
    // reaches a mutated job.
    resolve_resume_fallback_config(&args, snapshot.fallback.as_ref())?;

    // Cross-process liveness for plain resume (CLI-8): reuse the same launch
    // claim machinery the dashboard uses so concurrent resumes cannot
    // double-spend provider budget. A retry supervisor hands its claim to the
    // replacement worker via environment variables; a plain resume acquires a
    // fresh one. The claim is cleared by run_inner once the worker's control
    // watcher has established its runtime lease; every other exit path
    // releases it on drop.
    if !args.force {
        ensure_no_live_worker(&args.job_id)?;
    }
    let mut launch_claim = acquire_or_adopt_launch_claim(&args.job_id)?;
    // Check the lease again now that the claim is held: a worker that started
    // between the first check and the claim must not be double-run.
    if !args.force {
        ensure_no_live_worker(&args.job_id)?;
    }
    let progress_jsonl = args
        .progress_jsonl
        .clone()
        .or_else(|| snapshot.events_path.clone());
    // UI-2: hand the TUI the worker's cancel token so quitting an attached
    // `resume --ui tui` actually cancels the run at the next safe boundary
    // instead of leaving a headless worker spending in the background.
    let reporter = crate::progress::ProgressReporter::spawn_with_options(
        args.ui.unwrap_or(crate::progress::UiMode::Auto),
        progress_jsonl,
        true,
        Some(cancel_token.clone()),
    );
    let progress = reporter.sink();

    let run_result = run_inner(
        &args,
        &store,
        job,
        &mut snapshot,
        overrides,
        progress,
        cancel_token.clone(),
        Some(&mut launch_claim),
    )
    .await;
    if let Err(error) = &run_result {
        // Hard failures must not strand the job row in "running" forever
        // (CLI-5); an honest terminal status beats silent limbo.
        if let Ok(Some(record)) = store.get_job(&args.job_id)
            && matches!(record.status.as_str(), "running" | "queued")
        {
            tracing::warn!(
                job_id = %args.job_id,
                error = %error,
                "resume failed; marking job failed"
            );
            let _ = store.mark_job_failed(&args.job_id);
        }
    }
    let outcome = finalize_reporter(run_result, reporter).await;
    // Ctrl+C during resume now interrupts at the next safe boundary like
    // translate (CLI-3): cancel aborts provider calls and checkpoints drain,
    // so record the interrupted state instead of pretending nothing happened.
    if cancel_token.is_cancelled()
        && let Ok(Some(record)) = store.get_job(&args.job_id)
        && matches!(
            record.status.as_str(),
            "running" | "queued" | "retry_pending" | "interrupted"
        )
    {
        let _ = store.mark_job_interrupted(&args.job_id);
        if human_stdout_enabled(args.ui) {
            eprintln!();
            eprintln!("Interrupted by user.");
            eprintln!("Your progress has been saved to job: {}", args.job_id);
            eprintln!();
            eprintln!("Resume with:");
            eprintln!("  bookforge resume {}", args.job_id);
        }
        // UI-21: a user interruption is reported as 130 (128+SIGINT), not a
        // silent success — progress is saved, but the run did not finish.
        crate::exit_code::request(crate::exit_code::INTERRUPTED);
        return Ok(());
    }
    outcome
}

async fn live_fast_resume_paused_job(store: &JobStore, args: &ResumeArgs) -> Result<()> {
    let path = crate::control::request_job_control(&args.job_id, ControlCommand::Resume)?;
    if human_stdout_enabled(args.ui) {
        println!("resume requested for {} ({})", args.job_id, path.display());
    }
    if wait_for_paused_job_to_leave_paused(
        store,
        &args.job_id,
        PAUSED_RESUME_WAIT,
        PAUSED_RESUME_POLL,
    )
    .await?
    {
        return Ok(());
    }
    eprintln!("{}", dead_paused_resume_hint(&args.job_id));
    Ok(())
}

fn load_resume_overrides_for_status(
    job_id: &str,
    job_status: &str,
) -> Result<Option<reconfigure::RunConfigOverrides>> {
    match job_status {
        "paused" | "stopped" | "interrupted" | "retry_pending" | "needs_review" | "failed" => {
            reconfigure::load_overrides_for_job(job_id)
        }
        _ => Ok(None),
    }
}

async fn wait_for_paused_job_to_leave_paused(
    store: &JobStore,
    job_id: &str,
    timeout: Duration,
    poll_interval: Duration,
) -> Result<bool> {
    let started = tokio::time::Instant::now();
    loop {
        match store.get_job(job_id)? {
            Some(job) if job.status == "paused" => {}
            Some(_) | None => return Ok(true),
        }
        if started.elapsed() >= timeout {
            return Ok(false);
        }
        tokio::time::sleep(poll_interval).await;
    }
}

fn dead_paused_resume_hint(job_id: &str) -> String {
    format!(
        "Job {job_id} is still paused after waiting for a live process to resume it. \
If the paused process is gone, run: bookforge resume {job_id} --force. \
Only use --force if the paused process is gone; otherwise it can double-run the job."
    )
}

fn load_resume_snapshot(store: &JobStore, job_id: &str) -> Result<RunConfigSnapshot> {
    let Some(snapshot) = store.load_job_config_snapshot(job_id)? else {
        anyhow::bail!(
            "job '{}' does not have a run configuration snapshot; it cannot be resumed deterministically",
            job_id
        );
    };
    Ok(snapshot)
}

/// Complete-but-compatible snapshot guard: resume requires the fields that
/// make a deterministic re-run possible. Called before any status/control
/// mutation so an incomplete snapshot can never leave a half-resumed job.
fn validate_resume_snapshot(job_id: &str, snapshot: &RunConfigSnapshot) -> Result<()> {
    let mut missing = Vec::new();
    if snapshot.target_language.trim().is_empty() {
        missing.push("target language");
    }
    if snapshot.provider.trim().is_empty() {
        missing.push("provider");
    }
    if snapshot.model.trim().is_empty() {
        missing.push("model");
    }
    if snapshot.prompt_version.trim().is_empty() {
        missing.push("prompt version");
    }
    if snapshot.cache_namespace.trim().is_empty() {
        missing.push("cache namespace");
    }
    if !missing.is_empty() {
        anyhow::bail!(
            "job '{job_id}' has an incomplete run configuration snapshot; missing: {}",
            missing.join(", ")
        );
    }
    Ok(())
}

/// Resolve and sanity-check the output path before any mutation. Creates the
/// parent directory (idempotent) so a later rebuild cannot fail on a missing
/// parent after the job was already marked running.
fn resume_output_path(args: &ResumeArgs, snapshot: &RunConfigSnapshot) -> Result<PathBuf> {
    let output = args
        .output
        .clone()
        .unwrap_or_else(|| snapshot.output_path.clone());
    if output.is_dir() {
        anyhow::bail!("output path '{}' is a directory", output.display());
    }
    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|error| {
            anyhow::anyhow!(
                "cannot create output directory '{}': {error}",
                parent.display()
            )
        })?;
    }
    Ok(output)
}

/// Plain resume must not start a second worker beside a live one: a fresh
/// runtime lease (or an owner whose death cannot be established) refuses.
/// `--force` is the explicit escape hatch for a worker known to be gone.
fn ensure_no_live_worker(job_id: &str) -> Result<()> {
    if let crate::control::RuntimeLeaseState::Fresh(lease) =
        crate::control::runtime_lease_state(job_id, crate::control::RUNTIME_LEASE_STALE_AFTER)
    {
        anyhow::bail!(
            "job '{job_id}' already has an active worker (PID {}); stop that worker before resuming, or use --force only if it is gone",
            lease.pid
        );
    }
    Ok(())
}

/// Acquire the cross-process launch claim, adopting a parent's handoff when a
/// retry supervisor spawned this worker (closes the parent-to-child gap).
fn acquire_or_adopt_launch_claim(job_id: &str) -> Result<crate::control::RuntimeLaunchClaim> {
    if let Some(claim) = crate::control::RuntimeLaunchClaim::adopt_from_env(job_id)? {
        return Ok(claim);
    }
    let Some(claim) = crate::control::RuntimeLaunchClaim::acquire(job_id)? else {
        anyhow::bail!(
            "another bookforge process appears to be launching or resuming job '{job_id}'; \
             refusing to double-run the job"
        );
    };
    Ok(claim)
}

/// Run the resume pipeline under the caller's cancellation token.
///
/// Cancellation (Ctrl+C, or the TUI quitting) stops new dispatch through the
/// shared pause signal and aborts in-flight provider calls, but the pipeline
/// itself is never dropped: completed responses still reach the checkpoint
/// writer and its queue drains before this returns. A cancelled run records
/// `interrupted` and never publishes a book with source-text fallbacks. An
/// explicit Stop keeps precedence and leaves the job `stopped`.
#[allow(clippy::too_many_arguments)]
async fn run_inner(
    args: &ResumeArgs,
    store: &JobStore,
    job: JobRecord,
    snapshot: &mut RunConfigSnapshot,
    overrides: Option<reconfigure::RunConfigOverrides>,
    progress: Arc<dyn ProgressSink>,
    cancel_token: tokio_util::sync::CancellationToken,
    launch_claim: Option<&mut crate::control::RuntimeLaunchClaim>,
) -> Result<()> {
    let print_stdout = human_stdout_enabled(args.ui);
    let pause_signal = bookforge_llm::PauseSignal::new();
    // A control-file Stop cancels this child token without cancelling the
    // caller's, which is how an explicit stop is told apart from interruption.
    let stop_cancel_token = cancel_token.child_token();
    let result = if cancel_token.is_cancelled() {
        Ok(())
    } else {
        let pipeline = run_pipeline(
            args,
            store,
            &job,
            snapshot,
            overrides,
            progress,
            pause_signal.clone(),
            stop_cancel_token,
            launch_claim,
        );
        tokio::pin!(pipeline);
        tokio::select! {
            biased;
            _ = cancel_token.cancelled() => {
                pause_signal.stop();
                pipeline.await
            }
            result = &mut pipeline => result,
        }
    };
    if job_was_stopped(store, &job.id)? {
        print_stopped_resume_hint(&job.id, print_stdout);
    } else if cancel_token.is_cancelled() && result.is_ok() {
        store.mark_job_interrupted(&job.id)?;
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn run_pipeline(
    args: &ResumeArgs,
    store: &JobStore,
    job: &JobRecord,
    snapshot: &mut RunConfigSnapshot,
    overrides: Option<reconfigure::RunConfigOverrides>,
    progress: Arc<dyn ProgressSink>,
    pause_signal: bookforge_llm::PauseSignal,
    stop_cancel_token: tokio_util::sync::CancellationToken,
    launch_claim: Option<&mut crate::control::RuntimeLaunchClaim>,
) -> Result<()> {
    let control_path = bookforge_core::control_path_for_job(&job.id);
    let initial_control_revision = control_revision(&control_path)?;
    let started = std::time::Instant::now();
    progress.emit(ProgressEvent::StageStarted {
        stage: "resume".to_string(),
        timestamp_ms: now_ms(),
    });
    let input = resolve_job_input(job, snapshot)?;
    let output = args
        .output
        .clone()
        .unwrap_or_else(|| snapshot.output_path.clone());
    progress.emit(ProgressEvent::JobCreated {
        job_id: job.id.clone(),
        input_path: input.display().to_string(),
        output_path: output.display().to_string(),
        timestamp_ms: now_ms(),
    });
    let book = read_epub(&input)?;
    let mut settings = snapshot.settings.to_settings();
    let qa_mode = args
        .qa
        .or_else(|| overrides.as_ref().and_then(|overrides| overrides.qa))
        .unwrap_or_else(|| QaMode::from_snapshot(&snapshot.qa_mode));
    let strict_epubcheck = args.strict_epubcheck;
    let validate_output = args.validate_output
        || args.strict_epubcheck
        || overrides
            .as_ref()
            .and_then(|overrides| overrides.validate_output)
            .unwrap_or(snapshot.validate_output);
    if let Some(overrides) = overrides.as_ref() {
        reconfigure::apply_overrides_to_settings(&mut settings, overrides);
        // A successfully applied dashboard revision becomes the durable
        // baseline for later retry/resume waves. The sidecar is intentionally
        // cleared at terminal finalization, so failing to snapshot the merged
        // settings silently reverted the next retry to the original limits.
        snapshot.settings = ResolvedRunSettingsSnapshot::from_settings(&settings);
    }
    if let Some(value) = args.concurrency {
        settings.scheduler.concurrency = value.max(1);
    }
    if let Some(value) = args.max_attempts {
        settings.scheduler.max_attempts = value.max(1);
    }
    if let Some(value) = args.provider_max_attempts {
        settings.provider.provider_max_attempts = value.max(1);
    }
    if let Some(value) = args.validation_max_attempts {
        settings.provider.validation_max_attempts = value.max(1);
    }
    if let Some(value) = args.timeout_seconds {
        settings.provider.timeout_seconds = value;
    }
    if let Some(value) = args.max_output_tokens {
        settings.provider.max_output_tokens = Some(value);
    }
    if args.no_thinking {
        settings.provider.thinking_disabled = true;
    }
    // Explicit finalize-stage flags ride on top of the snapshot baseline so a
    // resumed run finalizes with the same QA/double-check/fallback controls a
    // fresh translate offers (CLI-16). Snapshot values stay authoritative for
    // anything the caller did not set.
    apply_resume_finalize_flag_overrides(&mut settings, args);
    progress.emit(ProgressEvent::RuntimeConfigResolved {
        profile: format!("{:?}", settings.profile),
        provider_preset: snapshot
            .provider_preset
            .as_ref()
            .map(|preset| format!("{preset:?}")),
        provider: snapshot.provider.clone(),
        model: snapshot.model.clone(),
        concurrency: settings.scheduler.concurrency,
        max_attempts: settings.scheduler.max_attempts,
        provider_max_attempts: settings.provider.provider_max_attempts,
        validation_max_attempts: settings.provider.validation_max_attempts,
        retry_after_policy: format!("{:?}", settings.provider.retry_after_policy),
        max_backoff_seconds: settings.provider.max_backoff_seconds,
        timeout_seconds: settings.provider.timeout_seconds,
        batch_enabled: settings.batch.enabled,
        batch_target_tokens: settings.batch.target_tokens,
        batch_max_items: settings.batch.max_items,
        adaptive_batch_sizing: settings.batch.adaptive_sizing,
        adaptive_concurrency: settings.adaptive_concurrency,
        compact_prompts: settings.compact_prompts,
        thinking_disabled: settings.provider.thinking_disabled,
        json_mode: format!("{:?}", settings.provider.json_mode),
        model_context_tokens: settings.provider.model_context_tokens,
        max_output_tokens: settings.provider.max_output_tokens,
        batch_max_output_tokens: settings.provider.batch_max_output_tokens,
        timestamp_ms: now_ms(),
    });
    let segments = build_segments(&book, &settings.segmentation)?;
    progress.emit(ProgressEvent::SegmentationFinished {
        segment_count: segments.len(),
        timestamp_ms: now_ms(),
    });
    let pending_ids = store.resumable_segment_ids(&job.id)?;

    let print_stdout = human_stdout_enabled(args.ui);
    if print_stdout {
        println!("Job: {}", job.id);
        println!("Input: {}", input.display());
        println!("Output: {}", output.display());
        println!("Provider: {}", job.provider);
        println!("Pending: {}", pending_ids.len());
    }

    let pending_segments = select_pending_segments(&segments, &pending_ids)?;
    let prompt_version = snapshot.prompt_version.as_str();
    let legacy_cache_namespace = snapshot.glossary_fingerprint.is_empty();
    let mut glossary = if legacy_cache_namespace {
        crate::commands::translate::PreparedGlossary {
            run_config: GlossaryRunConfig::default(),
            fingerprint: String::new(),
            active_terms: Vec::new(),
        }
    } else {
        let active_terms = merge_scope_terms(&snapshot.glossary_terms);
        let selected =
            select_glossary_for_segments(&segments, &active_terms, snapshot.glossary_budget_tokens);
        let fingerprint = crate::commands::translate::glossary_fingerprint(
            snapshot.glossary_format,
            snapshot.glossary_budget_tokens,
            snapshot.prompt_extra.as_deref(),
            &active_terms,
        );
        crate::commands::translate::PreparedGlossary {
            run_config: GlossaryRunConfig {
                format: snapshot.glossary_format,
                entries_by_segment: selected.entries_by_segment,
                prompt_extra: snapshot.prompt_extra.clone(),
                guidance_by_segment: std::collections::HashMap::new(),
            },
            fingerprint,
            active_terms,
        }
    };
    glossary.run_config.guidance_by_segment = store.load_retry_guidance(&job.id)?;
    let cache_namespace = if legacy_cache_namespace {
        compute_cache_namespace_v1(
            settings.segmentation.max_segment_tokens,
            settings.segmentation.context_tokens,
            settings.profile.namespace_str(),
            settings.batch.enabled,
            prompt_version,
        )
    } else {
        compute_cache_namespace(
            settings.segmentation.max_segment_tokens,
            settings.segmentation.context_tokens,
            settings.profile.namespace_str(),
            settings.batch.enabled,
            prompt_version,
            &glossary.fingerprint,
            if snapshot.style_rendered_block.is_empty() {
                ""
            } else {
                snapshot.style_fingerprint.as_str()
            },
            if snapshot.entities_rendered_block.is_empty() {
                ""
            } else {
                snapshot.entities_fingerprint.as_str()
            },
        )
    };
    if cache_namespace != snapshot.cache_namespace {
        anyhow::bail!(
            "resume cache namespace mismatch for job '{}': snapshot={}, recomputed={}",
            job.id,
            snapshot.cache_namespace,
            cache_namespace
        );
    }
    // The rebuilt contract matches, so this resume now owns the job. Until here
    // nothing was changed: a rejected resume leaves the status, the saved paths
    // and any pending control command exactly as they were.
    clear_preflight_control(&control_path, initial_control_revision)?;
    if (job.status == "paused" && args.force) || job.status == "stopped" {
        store.mark_job_running_for_resume(&job.id)?;
    }
    if args.output.is_some() {
        store.update_job_output_path(&job.id, &output)?;
        snapshot.output_path = output.clone();
    }
    if let Some(path) = args.progress_jsonl.clone() {
        store.update_job_event_path(&job.id, &path)?;
        snapshot.events_path = Some(path);
    }
    // Recheck accepted provider output only after the saved input and prompt
    // contract passed preflight, and before it can seed translation context.
    let revalidated = super::translate::revalidate_stored_translations(
        store,
        &job.id,
        &segments,
        snapshot.source_language.as_deref(),
        &snapshot.target_language,
    )?;
    if print_stdout && revalidated > 0 {
        println!(
            "Review: {revalidated} saved segment(s) contain untranslated source-language prose; kept for review."
        );
    }
    let context_cfg = snapshot_context_run_config(snapshot);
    let context_registry: Option<Arc<ContextRegistry>> = if context_cfg.enabled() {
        let registry = Arc::new(ContextRegistry::new(&segments));
        rehydrate_context_registry_from_store(&registry, &segments, store, &job.id)?;
        Some(registry)
    } else {
        None
    };
    // The Ctrl+C token from main feeds the same stop path the control-file
    // Stop command uses (CLI-3): `run_inner` stops the shared pause signal and
    // the child token cancels provider calls, so the run halts at safe
    // boundaries and never mid-checkpoint.
    let control_watcher = crate::control::ControlFileWatcher::spawn_with_stop_cancel(
        store.path().to_path_buf(),
        job.id.clone(),
        progress.clone(),
        pause_signal.clone(),
        stop_cancel_token.clone(),
        crate::control::ControlBaseline {
            settings: settings.clone(),
            qa: qa_mode,
            validate_output,
        },
    )?;
    // The worker's runtime lease now guards this job (the watcher wrote it
    // before starting), so the launch claim is no longer needed; clear it so a
    // stale claim can never block a later resume.
    if let Some(claim) = launch_claim {
        claim.clear();
    }
    let job_runtime_settings = control_watcher.job_runtime_settings();
    let run_config = TranslationRunConfig {
        source_language: snapshot.source_language.clone(),
        target_language: snapshot.target_language.clone(),
        provider: snapshot.provider.clone(),
        model: snapshot.model.clone(),
        prompt_version: snapshot.prompt_version.clone(),
        temperature: 0.2,
        scheduler: settings.scheduler.clone(),
        profile: settings.profile,
        model_context_tokens: settings.provider.model_context_tokens,
        max_output_tokens: settings.provider.max_output_tokens,
        batch_max_output_tokens: settings.provider.batch_max_output_tokens,
        compact_prompts: settings.compact_prompts,
        glossary: glossary.run_config.clone(),
        context: context_cfg,
        context_registry: context_registry.clone(),
        style: style_run_config_from_snapshot(snapshot),
        entities: entities_run_config_from_snapshot(snapshot),
        pause_signal: Some(pause_signal.clone()),
        runtime_settings: Some(control_watcher.runtime_settings()),
    };

    let retry_pending_ids = store
        .segment_records(&job.id)?
        .into_iter()
        .filter(|record| record.status == "retry_pending")
        .map(|record| record.id)
        .collect::<HashSet<_>>();
    let cacheable_pending_segments = pending_segments
        .iter()
        .filter(|segment| !retry_pending_ids.contains(&segment.id.0))
        .cloned()
        .collect::<Vec<_>>();
    let cached_translations = apply_cached_translations(
        &segments,
        &cacheable_pending_segments,
        CacheContext {
            store,
            job_id: &job.id,
            prompt_version,
            provider: &snapshot.provider,
            model: &snapshot.model,
            source_lang: snapshot.source_language.as_deref(),
            target_lang: &snapshot.target_language,
            cache_namespace: &cache_namespace,
        },
    )?;
    let pending_segments =
        select_pending_segments(&segments, &store.resumable_segment_ids(&job.id)?)?;
    progress.emit(ProgressEvent::CacheScanFinished {
        hits: cached_translations.len(),
        misses: pending_segments.len(),
        timestamp_ms: now_ms(),
    });
    emit_restored_progress(store, &job.id, &pending_segments, progress.as_ref())?;
    store.mark_job_running(&job.id)?;

    if !pending_segments.is_empty() {
        match job.provider.as_str() {
            "mock" => {
                let provider =
                    MockProvider::new(mock_mode(&snapshot.model), &snapshot.target_language);
                run_checkpointed_translation(
                    provider,
                    &pending_segments,
                    &run_config,
                    &settings,
                    CheckpointRunContext {
                        store,
                        job_id: &job.id,
                        provider: &snapshot.provider,
                        model: &snapshot.model,
                        prompt_version,
                    },
                    progress.clone(),
                    settings.batch.enabled,
                )
                .await
            }
            "deepseek" | "openrouter" | "openai-compatible" => {
                let provider_config = openai_compatible_config(job, snapshot, &settings)?;
                let provider = OpenAiCompatibleProvider::new_with_cancel(
                    provider_config,
                    stop_cancel_token.clone(),
                )?;
                run_checkpointed_translation(
                    provider,
                    &pending_segments,
                    &run_config,
                    &settings,
                    CheckpointRunContext {
                        store,
                        job_id: &job.id,
                        provider: &snapshot.provider,
                        model: &snapshot.model,
                        prompt_version,
                    },
                    progress.clone(),
                    settings.batch.enabled,
                )
                .await
            }
            provider => anyhow::bail!("cannot resume unsupported provider '{provider}'"),
        }?;
    }
    if job_was_stopped(store, &job.id)? {
        return Ok(());
    }

    let mut stored_blocks = store.load_block_translations(&job.id)?;
    let mut segment_records = store.segment_records(&job.id)?;
    let mut translations =
        rebuild_segment_translations(&segments, &stored_blocks, &segment_records);
    let mut control_poller = crate::control::ControlFilePoller::new_with_stop_cancel(
        store,
        &job.id,
        progress.clone(),
        stop_cancel_token.clone(),
    );
    if matches!(
        control_poller
            .wait_until_running_or_stopped(&pause_signal)
            .await?,
        bookforge_llm::PauseState::Stopped
    ) {
        return Ok(());
    }
    let qa_runtime = job_runtime_settings.borrow().clone();
    let qa_run_config = crate::control::freeze_run_config_for_stage(&run_config, &qa_runtime);
    let qa_reviews = qa_after_resume(
        job,
        &segments,
        &translations,
        &qa_run_config,
        snapshot,
        &qa_runtime.settings,
        qa_runtime.qa,
        args.qa_max_output_tokens,
        progress.clone(),
        stop_cancel_token.clone(),
    )
    .await?;
    if matches!(
        control_poller
            .wait_until_running_or_stopped(&pause_signal)
            .await?,
        bookforge_llm::PauseState::Stopped
    ) {
        return Ok(());
    }
    if qa_runtime.qa != QaMode::Off {
        persist_qa_reviews_best_effort(store, &job.id, &qa_reviews);
    }
    let fallback_config = resolve_resume_fallback_config(args, snapshot.fallback.as_ref())?;
    let fallback_translations = run_fallback_pass(
        &stop_cancel_token,
        fallback_config.as_ref(),
        &segments,
        translations,
        store,
        &job.id,
        prompt_version,
        &settings,
        &run_config,
        Some(&mut control_poller),
        progress.clone(),
        print_stdout,
    )
    .await?;
    translations = fallback_translations;
    if matches!(
        control_poller
            .wait_until_running_or_stopped(&pause_signal)
            .await?,
        bookforge_llm::PauseState::Stopped
    ) {
        return Ok(());
    }
    let double_check_runtime = job_runtime_settings.borrow().clone();
    let double_check_run_config =
        crate::control::freeze_run_config_for_stage(&run_config, &double_check_runtime);
    if double_check_runtime.settings.double_check.mode != DoubleCheckMode::Off
        && !snapshot.finalize.double_check_complete
    {
        // Human acceptance stays authoritative during optional automated
        // correction. The store refuses to overwrite a corrected row, but the
        // EPUB is rebuilt from in-memory translations, so corrected segments
        // must never enter the double-check pass at all.
        let human_corrected = store
            .load_terminal_segment_translations(&job.id)?
            .into_iter()
            .filter(|saved| saved.human_corrected)
            .map(|saved| saved.segment_id)
            .collect::<HashSet<_>>();
        let reviewable_segments = segments
            .iter()
            .filter(|segment| !human_corrected.contains(&segment.id.0))
            .cloned()
            .collect::<Vec<_>>();
        let changed_segment_ids = match double_check_after_resume(
            job,
            &reviewable_segments,
            &mut translations,
            &double_check_run_config,
            snapshot,
            &double_check_runtime.settings,
            progress.clone(),
            stop_cancel_token.clone(),
        )
        .await
        {
            Ok(changed_segment_ids) => changed_segment_ids,
            Err(_) if pause_signal.is_stopped() => {
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        persist_corrected_translations(
            store,
            &job.id,
            &double_check_run_config,
            &translations,
            &changed_segment_ids,
        )?;
        snapshot.finalize.double_check_complete = true;
        store.update_job_config_snapshot(&job.id, snapshot)?;
        stored_blocks = store.load_block_translations(&job.id)?;
        if matches!(
            control_poller
                .wait_until_running_or_stopped(&pause_signal)
                .await?,
            bookforge_llm::PauseState::Stopped
        ) {
            return Ok(());
        }
    }
    let block_translations = rebuild_block_translations(&segments, &stored_blocks, &translations);
    let rebuild_options = super::translate::rebuild_options_from_snapshot(snapshot);
    rebuild_epub_with_options(&book, &block_translations, &output, &rebuild_options)?;
    let validation_runtime = job_runtime_settings.borrow().clone();
    if validation_runtime.validate_output {
        let validation_path = validate::default_report_path(&output);
        let validation = validate::validate_and_write(&output, &validation_path, strict_epubcheck)?;
        if print_stdout {
            println!("Validation report: {}", validation_path.display());
        }
        if validation.failed {
            store.mark_job_failed(&job.id)?;
            anyhow::bail!(
                "rebuilt EPUB failed validation; see {}",
                validation_path.display()
            );
        }
    }
    loop {
        if matches!(
            control_poller
                .wait_until_running_or_stopped(&pause_signal)
                .await?,
            bookforge_llm::PauseState::Stopped
        ) {
            return Ok(());
        }
        if mark_job_finished(store, &job.id, &translations)? {
            break;
        }
        if job_was_stopped(store, &job.id)? {
            return Ok(());
        }
    }
    segment_records = store.segment_records(&job.id)?;

    let job = store
        .get_job(&job.id)?
        .ok_or_else(|| anyhow::anyhow!("job '{}' was not found after resume", job.id))?;
    let summary = store
        .summary(&job.id)?
        .ok_or_else(|| anyhow::anyhow!("job '{}' was not found after resume", job.id))?;
    let qa_inputs = translations
        .iter()
        .map(|translation| {
            TranslationQaInput::new(
                translation.segment_id.0.clone(),
                matches!(
                    translation.status,
                    SegmentStatus::Succeeded | SegmentStatus::SkippedCached
                ),
                translation.joined_text(),
            )
        })
        .collect::<Vec<_>>();
    let corrected_segments = store
        .load_terminal_segment_translations(&job.id)?
        .iter()
        .filter(|translation| translation.human_corrected)
        .count();
    let report = write_report(ReportInput {
        job: &job,
        summary: &summary,
        segments: &segments,
        segment_records: &segment_records,
        translations: &qa_inputs,
        qa_reviews: &qa_reviews,
        qa_findings: store.segment_qa_findings(&job.id)?,
        performance: snapshot
            .events_path
            .as_ref()
            .and_then(|path| performance_summary_from_events(path).ok().flatten()),
        output: &output,
        corrected_segments,
    })?;
    store.update_job_report_paths(&job.id, &report.json, &report.markdown)?;
    snapshot.report_json_path = Some(report.json.clone());
    snapshot.report_markdown_path = Some(report.markdown.clone());
    store.update_job_config_snapshot(&job.id, snapshot)?;
    reconfigure::clear_overrides_for_job(&job.id)?;
    progress.emit(ProgressEvent::ArtifactWritten {
        path: output.display().to_string(),
        timestamp_ms: now_ms(),
    });
    progress.emit(ProgressEvent::TranslationFinished {
        succeeded: summary.succeeded,
        cached: summary.cached,
        needs_review: summary.needs_review,
        failed: summary.failed,
        input_tokens: summary.input_tokens,
        output_tokens: summary.output_tokens,
        elapsed_ms: started.elapsed().as_millis() as u64,
        timestamp_ms: now_ms(),
    });

    if print_stdout {
        crate::commands::translate::print_run_summary(
            &summary,
            &job.provider,
            &job.model,
            &output,
            &report.markdown,
            &job.id,
        );
    }

    // UI-21: completing with unresolved segments is not a clean success; the
    // output EPUB is rebuilt, but scripts must be able to tell the difference.
    if summary.failed > 0 || summary.needs_review > 0 {
        crate::exit_code::request(crate::exit_code::COMPLETED_WITH_FAILURES);
    }

    Ok(())
}

/// Publish the job's saved checkpoint totals after the resume's cache scan, so
/// a viewer attached only to this run shows the whole job's progress. Prior
/// terminal statuses of segments about to be retried follow in bounded chunks,
/// letting their new outcome replace the old one instead of adding to it.
fn emit_restored_progress(
    store: &JobStore,
    job_id: &str,
    pending: &[Segment],
    progress: &dyn ProgressSink,
) -> Result<()> {
    const STATUS_CHUNK: usize = 64;
    let summary = store
        .summary(job_id)?
        .ok_or_else(|| anyhow::anyhow!("job '{job_id}' was not found"))?;
    progress.emit(ProgressEvent::ProgressRestored {
        total_segments: summary.total_segments,
        succeeded: summary.succeeded,
        cached: summary.cached,
        needs_review: summary.needs_review,
        failed: summary.failed,
        input_tokens: summary.input_tokens,
        output_tokens: summary.output_tokens,
        timestamp_ms: now_ms(),
    });
    let pending_ids = pending
        .iter()
        .map(|segment| segment.id.0.as_str())
        .collect::<HashSet<_>>();
    // Succeeded and cached checkpoints are not retried, so the summary already
    // covers them; only replaceable outcomes need per-segment bookkeeping.
    let statuses = store
        .segment_records(job_id)?
        .into_iter()
        .filter(|record| {
            pending_ids.contains(record.id.as_str())
                && matches!(
                    record.status.as_str(),
                    "succeeded" | "skipped_cached" | "needs_review" | "failed"
                )
        })
        .map(|record| (record.id, record.status))
        .collect::<Vec<_>>();
    for chunk in statuses.chunks(STATUS_CHUNK) {
        progress.emit(ProgressEvent::SegmentStatusesRestored {
            statuses: chunk.to_vec(),
            timestamp_ms: now_ms(),
        });
    }
    Ok(())
}

/// Identity of the control file's current contents (modification time and
/// length), or `None` when there is no control file.
fn control_revision(path: &Path) -> std::io::Result<Option<(SystemTime, u64)>> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(Some((metadata.modified()?, metadata.len()))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Clear the stale control command this resume started with. A Pause or Stop
/// written while the input was being rebuilt belongs to this invocation, even
/// when it repeats the old command, so a changed revision is left in place.
fn clear_preflight_control(path: &Path, before: Option<(SystemTime, u64)>) -> Result<()> {
    if control_revision(path)? == before {
        bookforge_core::clear_control_file(path)?;
    }
    Ok(())
}

fn human_stdout_enabled(ui: Option<crate::progress::UiMode>) -> bool {
    // Single source of truth in `UiMode` so `json-v1` cannot drift from the
    // UI-22 machine-stdout contract.
    ui.is_none_or(|mode| mode.human_stdout())
}

fn select_pending_segments(segments: &[Segment], pending_ids: &[String]) -> Result<Vec<Segment>> {
    let pending = pending_ids.iter().cloned().collect::<HashSet<_>>();
    let found = segments
        .iter()
        .filter(|segment| pending.contains(&segment.id.0))
        .cloned()
        .collect::<Vec<_>>();
    let found_ids = found
        .iter()
        .map(|segment| segment.id.0.as_str())
        .collect::<HashSet<_>>();
    let missing = pending_ids
        .iter()
        .filter(|id| !found_ids.contains(id.as_str()))
        .cloned()
        .collect::<Vec<_>>();

    if !missing.is_empty() {
        anyhow::bail!(
            "job references segment IDs that no longer exist after rebuilding the source IR: {}",
            missing.join(", ")
        );
    }

    Ok(found)
}

fn openai_compatible_config(
    job: &JobRecord,
    snapshot: &RunConfigSnapshot,
    settings: &ResolvedRunSettings,
) -> Result<OpenAiCompatibleConfig> {
    openai_compatible_config_from_parts(
        job.provider.as_str(),
        &snapshot.model,
        snapshot.base_url.as_deref(),
        snapshot.api_key_env.as_deref(),
        &job.id,
        settings,
    )
}

fn openai_compatible_config_from_parts(
    provider: &str,
    model: &str,
    base_url: Option<&str>,
    api_key_env: Option<&str>,
    job_id: &str,
    settings: &ResolvedRunSettings,
) -> Result<OpenAiCompatibleConfig> {
    let provider_max_attempts = settings.provider.provider_max_attempts.max(1);
    if provider == "deepseek" {
        let mut config = OpenAiCompatibleConfig::deepseek(Some(model.to_string()));
        if let Some(base_url) = base_url {
            config.base_url = base_url.to_string();
        }
        if let Some(api_key_env) = api_key_env {
            config.api_key_env = api_key_env.to_string();
        }
        config.timeout_seconds = settings.provider.timeout_seconds;
        config.provider_max_attempts = provider_max_attempts;
        config.thinking_disabled = settings.provider.thinking_disabled;
        config.retry_after_policy = settings.provider.retry_after_policy;
        config.max_backoff_seconds = settings.provider.max_backoff_seconds;
        config.max_idle_per_host = settings.provider.max_idle_per_host;
        config.json_mode = settings.provider.json_mode;
        return Ok(config);
    }

    if provider == "openrouter" {
        return Ok(OpenAiCompatibleConfig {
            base_url: base_url
                .map(String::from)
                .unwrap_or_else(|| "https://openrouter.ai/api/v1".to_string()),
            api_key_env: api_key_env
                .map(String::from)
                .unwrap_or_else(|| "OPENROUTER_API_KEY".to_string()),
            model: model.to_string(),
            timeout_seconds: settings.provider.timeout_seconds,
            provider_max_attempts,
            thinking_disabled: settings.provider.thinking_disabled,
            retry_after_policy: settings.provider.retry_after_policy,
            max_backoff_seconds: settings.provider.max_backoff_seconds,
            max_idle_per_host: settings.provider.max_idle_per_host,
            json_mode: settings.provider.json_mode,
        });
    }

    Ok(OpenAiCompatibleConfig {
        base_url: base_url.map(String::from).ok_or_else(|| {
            anyhow::anyhow!(
                "job '{}' does not have a stored base URL for openai-compatible resume",
                job_id
            )
        })?,
        api_key_env: api_key_env
            .map(String::from)
            .unwrap_or_else(|| "OPENAI_API_KEY".to_string()),
        model: model.to_string(),
        timeout_seconds: settings.provider.timeout_seconds,
        provider_max_attempts,
        thinking_disabled: settings.provider.thinking_disabled,
        retry_after_policy: settings.provider.retry_after_policy,
        max_backoff_seconds: settings.provider.max_backoff_seconds,
        max_idle_per_host: settings.provider.max_idle_per_host,
        json_mode: settings.provider.json_mode,
    })
}

fn snapshot_context_run_config(snapshot: &RunConfigSnapshot) -> ContextRunConfig {
    ContextRunConfig {
        window: snapshot.context_window,
        budget_tokens: snapshot.context_budget_tokens,
        scope: snapshot.context_scope,
        // Strictness is a scheduling choice for the resumed run's own
        // translation pass, so it is not rehydrated from the snapshot here.
        // The persisted CachePolicySnapshot (jobs.cache_policy_json) carries
        // the original strict-context choice and feeds the cache identity
        // used by lookups on resume.
        strict: false,
    }
}

fn style_run_config_from_snapshot(snapshot: &RunConfigSnapshot) -> Option<StyleRunConfig> {
    if snapshot.style_rendered_block.is_empty() {
        None
    } else {
        Some(StyleRunConfig {
            rendered_block: snapshot.style_rendered_block.clone(),
            fingerprint: snapshot.style_fingerprint.clone(),
        })
    }
}

fn entities_run_config_from_snapshot(snapshot: &RunConfigSnapshot) -> Option<EntityRunConfig> {
    if snapshot.entities_rendered_block.is_empty() {
        None
    } else {
        Some(EntityRunConfig {
            rendered_block: snapshot.entities_rendered_block.clone(),
            fingerprint: snapshot.entities_fingerprint.clone(),
        })
    }
}

fn rehydrate_context_registry_from_store(
    registry: &Arc<ContextRegistry>,
    segments: &[Segment],
    store: &JobStore,
    job_id: &str,
) -> Result<()> {
    let stored = store.load_terminal_segment_translations(job_id)?;
    let by_id: std::collections::HashMap<&str, &Segment> =
        segments.iter().map(|s| (s.id.0.as_str(), s)).collect();
    for record in &stored {
        let Some(segment) = by_id.get(record.segment_id.as_str()) else {
            continue;
        };
        let status = match record.status.as_str() {
            "succeeded" | "skipped_cached" => SegmentStatus::Succeeded,
            "needs_review" => SegmentStatus::NeedsReview,
            _ => SegmentStatus::Failed,
        };
        registry.pre_populate_text(segment, record.translated_text.clone(), status);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn qa_after_resume(
    job: &JobRecord,
    segments: &[Segment],
    translations: &[SegmentTranslation],
    config: &TranslationRunConfig,
    snapshot: &RunConfigSnapshot,
    settings: &ResolvedRunSettings,
    qa_mode: QaMode,
    qa_max_output_tokens: Option<u32>,
    progress: Arc<dyn ProgressSink>,
    stop_cancel_token: tokio_util::sync::CancellationToken,
) -> Result<Vec<QaSegmentReview>> {
    let qa_config = &settings.qa;
    let provider_name = qa_config.provider.as_deref().unwrap_or(&snapshot.provider);
    let model = qa_config.model.as_deref().unwrap_or(&snapshot.model);
    let base_url = qa_config
        .base_url
        .as_deref()
        .or(snapshot.base_url.as_deref());
    let api_key_env = qa_config
        .api_key_env
        .as_deref()
        .or(snapshot.api_key_env.as_deref());
    let mut qa_run_config = config.clone();
    qa_run_config.provider = provider_name.to_string();
    qa_run_config.model = model.to_string();

    match provider_name {
        "mock" => {
            let provider = MockProvider::new(mock_mode(model), &job.target_lang);
            Ok(qa_reviews_for_mode_with_max_output_tokens(
                ProgressRequestProvider::new(provider, progress),
                segments,
                translations,
                &qa_run_config,
                qa_config,
                qa_mode,
                qa_max_output_tokens,
            )
            .await)
        }
        "deepseek" | "openrouter" | "openai-compatible" => {
            let provider_config = openai_compatible_config_from_parts(
                provider_name,
                model,
                base_url,
                api_key_env,
                &job.id,
                settings,
            )?;
            let provider =
                OpenAiCompatibleProvider::new_with_cancel(provider_config, stop_cancel_token)?;
            Ok(qa_reviews_for_mode_with_max_output_tokens(
                ProgressRequestProvider::new(provider, progress),
                segments,
                translations,
                &qa_run_config,
                qa_config,
                qa_mode,
                qa_max_output_tokens,
            )
            .await)
        }
        _ => Ok(Vec::new()),
    }
}

#[allow(clippy::too_many_arguments)]
async fn double_check_after_resume(
    job: &JobRecord,
    segments: &[Segment],
    translations: &mut [SegmentTranslation],
    config: &TranslationRunConfig,
    snapshot: &RunConfigSnapshot,
    settings: &ResolvedRunSettings,
    progress: Arc<dyn ProgressSink>,
    stop_cancel_token: tokio_util::sync::CancellationToken,
) -> Result<Vec<String>> {
    // Nothing reviewable (e.g. every segment is human-accepted): construct no
    // provider and spend nothing.
    if segments.is_empty() {
        return Ok(Vec::new());
    }
    let double_check = &settings.double_check;
    let provider_name = double_check
        .provider
        .as_deref()
        .unwrap_or(&snapshot.provider);
    let model = double_check.model.as_deref().unwrap_or(&snapshot.model);
    let base_url = double_check
        .base_url
        .as_deref()
        .or(snapshot.base_url.as_deref());
    let api_key_env = double_check
        .api_key_env
        .as_deref()
        .or(snapshot.api_key_env.as_deref());

    let mut double_check_config = config.clone();
    double_check_config.provider = provider_name.to_string();
    double_check_config.model = model.to_string();

    let corrections = match provider_name {
        "mock" => {
            let provider = MockProvider::new(mock_mode(model), &job.target_lang);
            run_double_check(
                ProgressRequestProvider::new(provider, progress),
                segments,
                translations,
                &double_check_config,
                double_check,
            )
            .await
        }
        "deepseek" | "openrouter" | "openai-compatible" => {
            let provider_config = openai_compatible_config_from_parts(
                provider_name,
                model,
                base_url,
                api_key_env,
                &job.id,
                settings,
            )?;
            let provider =
                OpenAiCompatibleProvider::new_with_cancel(provider_config, stop_cancel_token)?;
            run_double_check(
                ProgressRequestProvider::new(provider, progress),
                segments,
                translations,
                &double_check_config,
                double_check,
            )
            .await
        }
        _ => return Ok(Vec::new()),
    }
    .map_err(|error| anyhow::anyhow!("double-check failed: {error}"))?;

    Ok(apply_double_check_corrections(translations, &corrections))
}

/// Override the snapshot-derived finalize settings with explicitly provided
/// resume flags. Snapshot values stay authoritative when a flag is unset, so
/// resumed runs keep their original QA/double-check/fallback configuration
/// unless the operator asks otherwise (CLI-16).
fn apply_resume_finalize_flag_overrides(settings: &mut ResolvedRunSettings, args: &ResumeArgs) {
    if let Some(value) = args.qa_concurrency {
        settings.qa.concurrency = value;
    }
    if let Some(value) = args.qa_batch_target_tokens {
        settings.qa.batch_target_tokens = value;
    }
    if let Some(value) = &args.qa_model {
        settings.qa.model = Some(value.clone());
    }
    if let Some(value) = &args.qa_provider {
        settings.qa.provider = Some(value.clone());
    }
    if let Some(value) = &args.qa_base_url {
        settings.qa.base_url = Some(value.clone());
    }
    if let Some(value) = &args.qa_api_key_env {
        settings.qa.api_key_env = Some(value.clone());
    }

    if let Some(mode) = args.double_check {
        settings.double_check.mode = mode;
    }
    if let Some(model) = &args.double_check_model {
        settings.double_check.model = Some(model.clone());
    }
    if let Some(provider) = &args.double_check_provider {
        settings.double_check.provider = Some(provider.clone());
    }
    if let Some(base_url) = &args.double_check_base_url {
        settings.double_check.base_url = Some(base_url.clone());
    }
    if let Some(api_key_env) = &args.double_check_api_key_env {
        settings.double_check.api_key_env = Some(api_key_env.clone());
    }
    if let Some(concurrency) = args.double_check_concurrency {
        settings.double_check.concurrency = concurrency;
    }
}

/// Build the fallback-pass config from explicit resume flags when any are
/// provided; otherwise inherit the snapshot's recorded fallback verbatim.
fn resolve_resume_fallback_config(
    args: &ResumeArgs,
    snapshot: Option<&bookforge_core::FallbackRunConfigSnapshot>,
) -> Result<Option<FallbackPassConfig>> {
    let explicit = args.fallback_provider.is_some()
        || args.fallback_model.is_some()
        || args.fallback_base_url.is_some()
        || args.fallback_api_key_env.is_some()
        || args.fallback_only.is_some();
    if !explicit {
        return Ok(FallbackPassConfig::from_snapshot(snapshot));
    }
    let inherited = snapshot;
    let provider = args
        .fallback_provider
        .clone()
        .or_else(|| inherited.map(|fallback| fallback.provider.clone()))
        .unwrap_or_else(|| "deepseek".to_string());
    let model = args
        .fallback_model
        .clone()
        .or_else(|| inherited.map(|fallback| fallback.model.clone()))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "--fallback-provider requires --fallback-model (or an original run with --fallback-provider/--fallback-model)"
            )
        })?;
    Ok(Some(FallbackPassConfig {
        provider,
        model,
        base_url: args
            .fallback_base_url
            .clone()
            .or_else(|| inherited.and_then(|fallback| fallback.base_url.clone())),
        api_key_env: args
            .fallback_api_key_env
            .clone()
            .or_else(|| inherited.and_then(|fallback| fallback.api_key_env.clone())),
        scope: args.fallback_only.unwrap_or_else(|| {
            inherited
                .map(|fallback| fallback.scope)
                .unwrap_or(FallbackScope::Failed)
        }),
    }))
}

fn rebuild_block_translations(
    segments: &[Segment],
    stored: &[StoredBlockTranslation],
    fresh: &[SegmentTranslation],
) -> Vec<BlockTranslation> {
    let mut by_block = HashMap::<String, String>::new();
    for translation in stored {
        by_block.insert(translation.block_id.clone(), translation.text.clone());
    }
    for translation in fresh {
        for block in &translation.blocks {
            by_block.insert(block.block_id.0.clone(), block.text.clone());
        }
    }

    let mut blocks = Vec::new();
    for segment in segments {
        for block in &segment.source.blocks {
            blocks.push(BlockTranslation {
                block_id: block.block_id.clone(),
                text: by_block
                    .get(&block.block_id.0)
                    .cloned()
                    .unwrap_or_else(|| block.text.clone()),
            });
        }
    }
    blocks
}

fn rebuild_segment_translations(
    segments: &[Segment],
    stored: &[StoredBlockTranslation],
    records: &[bookforge_store::SegmentRecord],
) -> Vec<SegmentTranslation> {
    let mut by_segment_block = HashMap::<(String, String), String>::new();
    for translation in stored {
        by_segment_block.insert(
            (translation.segment_id.clone(), translation.block_id.clone()),
            translation.text.clone(),
        );
    }
    let status_by_segment = records
        .iter()
        .map(|record| {
            (
                record.id.as_str(),
                (record.status.as_str(), record.error.clone()),
            )
        })
        .collect::<HashMap<_, _>>();

    let mut translations = Vec::new();
    for segment in segments {
        let mut blocks = Vec::new();
        for block in &segment.source.blocks {
            if let Some(text) =
                by_segment_block.get(&(segment.id.0.clone(), block.block_id.0.clone()))
            {
                blocks.push(BlockTranslation {
                    block_id: block.block_id.clone(),
                    text: text.clone(),
                });
            }
        }
        let stored_status = status_by_segment
            .get(segment.id.0.as_str())
            .cloned()
            .unwrap_or(("succeeded", None));
        if blocks.is_empty() {
            // A terminal segment can exist with a status row but NO stored
            // blocks when its last attempt failed again during this resume.
            // Emitting a status-only entry keeps the completion decision, QA
            // inputs, and report honest instead of silently omitting the
            // segment and green-lighting raw source placeholders (H-4).
            // Pending statuses stay hidden: those are retried later waves.
            if matches!(
                stored_status.0,
                "succeeded" | "skipped_cached" | "needs_review" | "failed"
            ) {
                translations.push(SegmentTranslation {
                    segment_id: segment.id.clone(),
                    ordinal: segment.ordinal,
                    block_ids: segment.block_ids.clone(),
                    blocks: Vec::new(),
                    checksum: segment.checksum.clone(),
                    status: segment_status(stored_status.0),
                    template: "stored".to_string(),
                    error: stored_status.1,
                    input_tokens: None,
                    input_cached_tokens: None,
                    output_tokens: None,
                    tokens_estimated: false,
                    findings: Vec::new(),
                });
            }
            continue;
        }
        let (status, error) = stored_status;
        translations.push(SegmentTranslation {
            segment_id: segment.id.clone(),
            ordinal: segment.ordinal,
            block_ids: segment.block_ids.clone(),
            blocks,
            checksum: segment.checksum.clone(),
            status: segment_status(status),
            template: "stored".to_string(),
            error,
            input_tokens: None,
            input_cached_tokens: None,
            output_tokens: None,
            tokens_estimated: false,
            findings: Vec::new(),
        });
    }

    translations
}

fn segment_status(status: &str) -> SegmentStatus {
    match status {
        "skipped_cached" => SegmentStatus::SkippedCached,
        "needs_review" => SegmentStatus::NeedsReview,
        "failed" => SegmentStatus::Failed,
        _ => SegmentStatus::Succeeded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookforge_core::{
        JsonMode, TranslationProfile,
        ir::{BlockId, SectionId},
        segment::{
            SegmentBlock, SegmentConstraints, SegmentContext, SegmentId, SegmentMetadata,
            SegmentSource, SegmentTextRun,
        },
    };
    use bookforge_store::{CreateJob, SaveManualCorrection, SaveNeedsReview, SaveTranslation};
    use std::{
        io::Write,
        path::{Path, PathBuf},
        sync::Mutex,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };
    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    struct RecordingSink {
        events: Arc<Mutex<Vec<ProgressEvent>>>,
    }

    impl ProgressSink for RecordingSink {
        fn emit(&self, event: ProgressEvent) {
            self.events.lock().expect("events lock").push(event);
        }
    }

    struct ResumeFixture {
        _tempdir: tempfile::TempDir,
        store: JobStore,
        job: JobRecord,
        snapshot: RunConfigSnapshot,
        segments: Vec<Segment>,
    }

    fn assert_future_pending<F: std::future::Future>(mut future: std::pin::Pin<&mut F>) {
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(
            future.as_mut().poll(&mut context).is_pending(),
            "future should wait for the test transition"
        );
    }

    #[tokio::test]
    async fn resume_marks_job_needs_review_when_segments_fail_again() {
        // H-4 regression: a provider whose responses fail validation on every
        // attempt means the resumed segment fails again during the resume
        // pass. Before the fix, the resumed-completion decision looked only at
        // rebuilt translations and could green-light a job whose output still
        // contained raw source placeholders.
        let mut settings = TranslationProfile::V1Fast.resolve();
        settings.batch.enabled = false;
        let mut fixture = resume_fixture(settings, 2);
        save_succeeded(
            &fixture.store,
            &fixture.job.id,
            &fixture.segments[0],
            "stored",
        );
        fixture.snapshot.model = "mock-malformed-json".to_string();
        fixture
            .store
            .update_job_config_snapshot(&fixture.job.id, &fixture.snapshot)
            .expect("snapshot should persist");

        let events = run_fixture(&mut fixture)
            .await
            .expect("resume should complete honestly instead of aborting");

        let record = fixture
            .store
            .get_job(&fixture.job.id)
            .expect("job lookup")
            .expect("job exists");
        assert_eq!(
            record.status, "needs_review",
            "a resume whose segment failed again must not report success"
        );
        let summary = fixture
            .store
            .summary(&fixture.job.id)
            .expect("summary loads")
            .expect("summary exists");
        assert_eq!(summary.succeeded, 1);
        assert_eq!(summary.needs_review, 1);
        let finished = events.iter().any(|event| {
            matches!(
                event,
                ProgressEvent::TranslationFinished { needs_review, .. } if *needs_review > 0
            )
        });
        assert!(
            finished,
            "completion event should carry the review counts honestly"
        );
    }

    #[test]
    fn completion_decision_includes_blockless_failed_rows_from_the_db() {
        // The heart of H-4: a 'failed' segment row WITHOUT stored blocks —
        // exactly what the checkpoint writer leaves behind when an attempt
        // fails hard during resume — is invisible if you look only at rebuilt
        // translations. mark_job_finished must consult the DB summary and
        // refuse to green-light such a book.
        let mut settings = TranslationProfile::V1Fast.resolve();
        settings.batch.enabled = false;
        let fixture = resume_fixture(settings.clone(), 2);
        save_succeeded(
            &fixture.store,
            &fixture.job.id,
            &fixture.segments[0],
            "stored",
        );
        fixture
            .store
            .mark_segment_failed_if_unfinished(
                &fixture.job.id,
                &fixture.segments[1].id.0,
                "transport died mid-resume",
            )
            .expect("failed row written like the checkpoint writer does");

        // Reproduce exactly what run_inner feeds mark_job_finished.
        let book = read_epub(&fixture.snapshot.input_path).expect("fixture EPUB should read");
        let segments = build_segments(&book, &settings.segmentation).expect("segments build");
        let stored_blocks = fixture
            .store
            .load_block_translations(&fixture.job.id)
            .expect("blocks load");
        let segment_records = fixture
            .store
            .segment_records(&fixture.job.id)
            .expect("records load");
        let translations =
            rebuild_segment_translations(&segments, &stored_blocks, &segment_records);

        assert!(
            translations
                .iter()
                .any(|translation| translation.status == SegmentStatus::Failed),
            "rebuilt translations must surface the blockless failed segment"
        );

        let completed_or_review = mark_job_finished(&fixture.store, &fixture.job.id, &translations)
            .expect("completion decision");
        assert!(
            completed_or_review,
            "with no live pause/stop control the decision must finalize"
        );
        let record = fixture
            .store
            .get_job(&fixture.job.id)
            .expect("job lookup")
            .expect("job exists");
        assert_eq!(
            record.status, "needs_review",
            "blockless failed rows must block a green light"
        );
    }

    #[test]
    fn rebuild_segment_translations_surfaces_blockless_terminal_segments() {
        let segments = vec![test_segment("seg_blockless", 0), test_segment("seg_ok", 1)];
        let records = vec![bookforge_store::SegmentRecord {
            id: "seg_blockless".to_string(),
            status: "failed".to_string(),
            attempts: 3,
            error: Some("provider rejected batch".to_string()),
            input_tokens: None,
            input_cached_tokens: None,
            output_tokens: None,
            tokens_estimated: false,
        }];
        let stored = vec![StoredBlockTranslation {
            segment_id: "seg_ok".to_string(),
            block_id: "b_000001".to_string(),
            text: "stored ok".to_string(),
        }];

        let rebuilt = rebuild_segment_translations(&segments, &stored, &records);
        let by_id = rebuilt
            .iter()
            .map(|translation| (translation.segment_id.0.as_str(), translation))
            .collect::<HashMap<_, _>>();

        let failed = by_id.get("seg_blockless").expect("failed entry present");
        assert_eq!(
            failed.status,
            SegmentStatus::Failed,
            "blockless terminal segments must surface their status"
        );
        assert_eq!(
            failed.error.as_deref(),
            Some("provider rejected batch"),
            "the failure reason should ride along"
        );
        assert!(failed.blocks.is_empty());
        assert_eq!(
            by_id.get("seg_ok").expect("stored entry present").status,
            SegmentStatus::Succeeded
        );

        // A pending record without blocks stays invisible (it is work-in-
        // progress, not a terminal outcome).
        let rebuilt_pending_only = rebuild_segment_translations(
            &[test_segment("seg_pending", 2)],
            &[],
            &[bookforge_store::SegmentRecord {
                id: "seg_pending".to_string(),
                status: "retry_pending".to_string(),
                attempts: 1,
                error: None,
                input_tokens: None,
                input_cached_tokens: None,
                output_tokens: None,
                tokens_estimated: false,
            }],
        );
        assert!(
            rebuilt_pending_only.is_empty(),
            "pending statuses without blocks should not become completion inputs"
        );
    }

    #[tokio::test]
    async fn paused_resume_wait_times_out_with_force_hint() {
        let fixture = resume_fixture(TranslationProfile::V1Fast.resolve(), 1);
        fixture
            .store
            .mark_job_paused(&fixture.job.id)
            .expect("job should mark paused");

        let resumed = wait_for_paused_job_to_leave_paused(
            &fixture.store,
            &fixture.job.id,
            Duration::from_millis(1),
            Duration::from_millis(1),
        )
        .await
        .expect("paused wait should not error");

        assert!(!resumed, "dead paused job should time out");
        let hint = dead_paused_resume_hint(&fixture.job.id);
        assert!(hint.contains("bookforge resume"));
        assert!(hint.contains("--force"));
        assert!(hint.contains("double-run"));
    }

    #[tokio::test]
    async fn paused_resume_wait_returns_when_job_leaves_paused() {
        let fixture = resume_fixture(TranslationProfile::V1Fast.resolve(), 1);
        fixture
            .store
            .mark_job_paused(&fixture.job.id)
            .expect("job should mark paused");
        let job_id = fixture.job.id.clone();

        let resumed = wait_for_paused_job_to_leave_paused(
            &fixture.store,
            &job_id,
            Duration::from_secs(1),
            Duration::from_millis(1),
        );
        tokio::pin!(resumed);
        assert_future_pending(resumed.as_mut());
        fixture
            .store
            .mark_job_running(&job_id)
            .expect("job should leave paused");
        let resumed = resumed.await.expect("paused wait should not error");

        assert!(resumed, "live paused job should be observed leaving paused");
    }

    #[tokio::test]
    async fn paused_fast_resume_with_overrides_signals_the_live_watcher() {
        let fixture = resume_fixture(TranslationProfile::V1Fast.resolve(), 1);
        fixture
            .store
            .mark_job_paused(&fixture.job.id)
            .expect("job should mark paused");
        let _path = write_overrides_sidecar(
            &fixture.job.id,
            &reconfigure::RunConfigOverrides {
                batch_max_output_tokens: Some(12_000),
                ..reconfigure::RunConfigOverrides::default()
            },
        );
        let args = resume_args(&fixture.job.id);
        let job_id = fixture.job.id.clone();

        let resume = live_fast_resume_paused_job(&fixture.store, &args);
        tokio::pin!(resume);
        assert_future_pending(resume.as_mut());
        assert_eq!(
            bookforge_core::read_control_file(&bookforge_core::control_path_for_job(&job_id))
                .expect("control file should read"),
            ControlCommand::Resume
        );
        fixture
            .store
            .mark_job_running(&job_id)
            .expect("live watcher should leave paused");
        resume
            .await
            .expect("pending overrides should be applied by the live watcher");

        assert_eq!(
            bookforge_core::read_control_file(&bookforge_core::control_path_for_job(&job_id))
                .expect("control file should read"),
            ControlCommand::Resume
        );
        reconfigure::clear_overrides_for_job(&fixture.job.id).expect("sidecar should clear");
    }

    #[tokio::test]
    async fn resume_uses_snapshot_segmentation_settings() {
        let mut settings = TranslationProfile::V1Fast.resolve();
        settings.batch.enabled = false;
        settings.segmentation.max_segment_tokens = 1377;
        settings.segmentation.context_tokens = 77;
        let mut fixture = resume_fixture(settings, 1);

        run_fixture(&mut fixture)
            .await
            .expect("resume should use snapshot segmentation settings");

        let summary = fixture
            .store
            .summary(&fixture.job.id)
            .expect("summary should load")
            .expect("job should exist");
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.succeeded, 1);
    }

    #[tokio::test]
    async fn resume_uses_snapshot_profile_and_compact_prompt_settings() {
        let mut settings = TranslationProfile::Safe.resolve();
        settings.batch.enabled = false;
        settings.compact_prompts = true;
        let mut fixture = resume_fixture(settings, 1);

        let events = run_fixture(&mut fixture)
            .await
            .expect("resume should succeed");
        let runtime = runtime_config_event(&events);

        assert_eq!(runtime.profile, "Safe");
        assert!(runtime.compact_prompts);
    }

    #[tokio::test]
    async fn resume_uses_snapshot_provider_json_mode_and_attempts() {
        let mut settings = TranslationProfile::V1Fast.resolve();
        settings.batch.enabled = false;
        settings.provider.json_mode = JsonMode::PromptOnly;
        settings.provider.provider_max_attempts = 4;
        let mut fixture = resume_fixture(settings.clone(), 1);

        let config = openai_compatible_config_from_parts(
            "openrouter",
            "snapshot-model",
            Some("https://snapshot.example/v1"),
            Some("SNAPSHOT_API_KEY"),
            &fixture.job.id,
            &settings,
        )
        .expect("provider config should build from snapshot settings");
        assert_eq!(config.json_mode, JsonMode::PromptOnly);
        assert_eq!(config.provider_max_attempts, 4);

        let events = run_fixture(&mut fixture)
            .await
            .expect("resume should succeed");
        let runtime = runtime_config_event(&events);
        assert_eq!(runtime.json_mode, "PromptOnly");
        assert_eq!(runtime.provider_max_attempts, 4);
    }

    #[tokio::test]
    async fn resume_applies_reconfigure_overrides_without_retranslating_succeeded_segments() {
        let mut settings = TranslationProfile::V1Fast.resolve();
        settings.batch.enabled = true;
        settings.batch.target_tokens = 8_000;
        settings.batch.max_items = 8;
        settings.batch.adaptive_sizing = true;
        settings.provider.batch_max_output_tokens = Some(2_000);
        settings.provider.provider_max_attempts = 2;
        let mut fixture = resume_fixture(settings, 2);
        save_succeeded(
            &fixture.store,
            &fixture.job.id,
            &fixture.segments[0],
            "stored",
        );
        fixture
            .store
            .mark_segment_failed(&fixture.job.id, &fixture.segments[1].id.0, "retry")
            .expect("second segment should be retryable");

        let events = run_fixture_with_overrides(
            &mut fixture,
            Some(reconfigure::RunConfigOverrides {
                batch_max_output_tokens: Some(12_000),
                batch_max_items: Some(1),
                batch_target_tokens: Some(4_000),
                provider_max_attempts: Some(5),
                adaptive_concurrency: Some(true),
                adaptive_batch_sizing: Some(false),
                ..reconfigure::RunConfigOverrides::default()
            }),
        )
        .await
        .expect("resume should apply reconfigure overrides");
        let runtime = runtime_config_event(&events);

        assert_eq!(runtime.batch_max_output_tokens, Some(12_000));
        assert_eq!(runtime.batch_max_items, 1);
        assert_eq!(runtime.batch_target_tokens, 4_000);
        assert_eq!(runtime.provider_max_attempts, 5);
        assert!(runtime.adaptive_concurrency);
        assert!(!runtime.adaptive_batch_sizing);
        assert_eq!(
            segment_finished_ids(&events),
            vec![fixture.segments[1].id.0.clone()]
        );
        let stored = fixture
            .store
            .load_block_translations(&fixture.job.id)
            .expect("stored translations should load");
        assert!(
            stored
                .iter()
                .any(|block| block.segment_id == fixture.segments[0].id.0
                    && block.text.starts_with("stored")),
            "succeeded segment should keep its original checkpointed translation"
        );
    }

    #[tokio::test]
    async fn resume_clears_overrides_after_success_and_terminal_status_ignores_stale_sidecar() {
        let mut settings = TranslationProfile::V1Fast.resolve();
        settings.batch.enabled = false;
        settings.provider.batch_max_output_tokens = Some(2_000);
        let mut fixture = resume_fixture(settings, 1);
        let overrides = reconfigure::RunConfigOverrides {
            batch_max_output_tokens: Some(12_000),
            ..reconfigure::RunConfigOverrides::default()
        };
        let path = write_overrides_sidecar(&fixture.job.id, &overrides);

        let events = run_fixture_with_overrides(&mut fixture, Some(overrides))
            .await
            .expect("resume should apply overrides and finish");
        let runtime = runtime_config_event(&events);

        assert_eq!(runtime.batch_max_output_tokens, Some(12_000));
        let persisted = fixture
            .store
            .load_job_config_snapshot(&fixture.job.id)
            .expect("snapshot lookup should work")
            .expect("snapshot should remain present");
        assert_eq!(
            persisted.settings.provider.batch_max_output_tokens,
            Some(12_000),
            "applied overrides must survive later retry waves"
        );
        assert!(
            !path.exists(),
            "successful terminal resume should remove overrides sidecar"
        );

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("sidecar parent should exist");
        }
        std::fs::write(&path, "{not valid json").expect("stale malformed sidecar should write");
        let overrides = load_resume_overrides_for_status(&fixture.job.id, "succeeded")
            .expect("terminal job should not read stale sidecar");

        assert!(overrides.is_none());
        assert!(
            path.exists(),
            "terminal status guard should not consume the stale sidecar"
        );
        reconfigure::clear_overrides_for_job(&fixture.job.id)
            .expect("stale sidecar should clean up");
    }

    #[test]
    fn incomplete_resume_loads_a_durable_override_sidecar() {
        let job_id = format!("retry_pending_overrides_{}", std::process::id());
        let path = write_overrides_sidecar(
            &job_id,
            &reconfigure::RunConfigOverrides {
                batch_max_output_tokens: Some(4_096),
                ..reconfigure::RunConfigOverrides::default()
            },
        );

        for status in ["retry_pending", "interrupted"] {
            let loaded = load_resume_overrides_for_status(&job_id, status)
                .expect("incomplete resume should read its sidecar")
                .expect("override should be present");
            assert_eq!(loaded.batch_max_output_tokens, Some(4_096), "{status}");
        }

        reconfigure::clear_overrides_for_job(&job_id).expect("sidecar should clean up");
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn resume_missing_config_snapshot_fails_clearly() {
        let tempdir = tempfile::tempdir().expect("tempdir should be created");
        let input = fixture_input();
        let output = tempdir.path().join("out.epub");
        let store = JobStore::open(tempdir.path().join("jobs.sqlite")).expect("store should open");
        let job = store
            .create_job(CreateJob {
                input: &input,
                output: &output,
                source_lang: Some("English"),
                target_lang: "Italian",
                provider: "mock",
                model: "mock-prefix-target",
                base_url: None,
                api_key_env: None,
                book_id: None,
                series_id: None,
            })
            .expect("job should be created");

        let error = load_resume_snapshot(&store, &job.id)
            .expect_err("missing snapshot should fail clearly");

        assert!(error.to_string().contains("run configuration snapshot"));
        assert!(
            error
                .to_string()
                .contains("cannot be resumed deterministically")
        );
    }

    #[tokio::test]
    async fn resume_reuses_checkpointed_segments_and_translates_only_resumable_segments() {
        let mut settings = TranslationProfile::V1Fast.resolve();
        settings.batch.enabled = false;
        let mut fixture = resume_fixture(settings, 2);
        save_succeeded(
            &fixture.store,
            &fixture.job.id,
            &fixture.segments[0],
            "stored",
        );
        fixture
            .store
            .mark_segment_failed(&fixture.job.id, &fixture.segments[1].id.0, "retry")
            .expect("segment should be failed");

        let events = run_fixture(&mut fixture)
            .await
            .expect("resume should succeed");
        let finished = segment_finished_ids(&events);

        assert_eq!(finished, vec![fixture.segments[1].id.0.clone()]);
        let summary = fixture
            .store
            .summary(&fixture.job.id)
            .expect("summary should load")
            .expect("job should exist");
        assert_eq!(summary.succeeded, 2);
        assert_eq!(summary.failed, 0);

        // A viewer attached only to this resume sees the job's saved progress
        // before any fresh work finishes, and the retried failure is replaced
        // by its new outcome rather than counted twice.
        let first_finished = events
            .iter()
            .position(|event| matches!(event, ProgressEvent::SegmentFinished { .. }))
            .unwrap();
        let restored =
            bookforge_core::RunState::from_events(events[..first_finished].iter().cloned());
        assert_eq!(restored.total_segments, 2);
        assert_eq!(
            (restored.succeeded, restored.cached, restored.failed),
            (1, 0, 1)
        );
        assert_eq!(restored.run_total_segments, 1);
        assert_eq!(restored.run_completed_segments, 0);
        let restored_statuses = events
            .iter()
            .filter_map(|event| match event {
                ProgressEvent::SegmentStatusesRestored { statuses, .. } => Some(statuses.clone()),
                _ => None,
            })
            .flatten()
            .collect::<Vec<_>>();
        assert_eq!(
            restored_statuses,
            vec![(fixture.segments[1].id.0.clone(), "failed".into())]
        );
        let mut updated = restored;
        updated.fold(&events[first_finished]);
        assert_eq!(
            (updated.done_segments, updated.succeeded, updated.failed),
            (2, 2, 0)
        );

        // Replaying the original run's log first must give the same view.
        let attached = bookforge_core::RunState::from_events(events.clone());
        let mut replayed = bookforge_core::RunState::from_events([
            ProgressEvent::JobCreated {
                job_id: fixture.job.id.clone(),
                input_path: "old.epub".into(),
                output_path: "old-out.epub".into(),
                timestamp_ms: 1_000,
            },
            ProgressEvent::SegmentFinished {
                segment_id: fixture.segments[0].id.0.clone(),
                status: "succeeded".into(),
                input_tokens: Some(1),
                output_tokens: Some(1),
                timestamp_ms: 2_000,
            },
        ]);
        for event in &events {
            replayed.fold(event);
        }
        assert_eq!(attached.done_segments, replayed.done_segments);
        assert_eq!(attached.elapsed_secs(), replayed.elapsed_secs());
        assert_eq!(
            attached.segments_per_minute(),
            replayed.segments_per_minute()
        );
        assert_eq!(
            (attached.input_tokens, attached.output_tokens),
            (replayed.input_tokens, replayed.output_tokens)
        );
        assert_eq!(attached.run_completed_segments, 1);
    }

    #[tokio::test]
    async fn resume_retry_pending_bypasses_cache() {
        let mut settings = TranslationProfile::V1Fast.resolve();
        settings.batch.enabled = false;
        let mut fixture = resume_fixture(settings, 1);
        let cache_job = fixture
            .store
            .create_job(CreateJob {
                input: &fixture.snapshot.input_path,
                output: &fixture._tempdir.path().join("cache-output.epub"),
                source_lang: Some("English"),
                target_lang: "Italian",
                provider: "mock",
                model: "mock-prefix-target",
                base_url: None,
                api_key_env: None,
                book_id: None,
                series_id: None,
            })
            .expect("cache job should be created");
        fixture
            .store
            .insert_segments(
                &cache_job.id,
                &fixture.segments,
                fixture.snapshot.prompt_version.as_str(),
                "mock",
                "mock-prefix-target",
                fixture.snapshot.cache_namespace.as_str(),
            )
            .expect("cache job segments should insert");
        save_succeeded(
            &fixture.store,
            &cache_job.id,
            &fixture.segments[0],
            "stale cached text",
        );
        fixture
            .store
            .mark_segment_failed(&fixture.job.id, &fixture.segments[0].id.0, "retry")
            .expect("segment should be failed");
        fixture
            .store
            .retry_segments(&fixture.job.id, bookforge_store::RetryScope::Failed)
            .expect("segment should become retry-pending");

        let events = run_fixture(&mut fixture)
            .await
            .expect("resume should freshly translate retry-pending segments");
        let finished = segment_finished_ids(&events);

        assert_eq!(finished, vec![fixture.segments[0].id.0.clone()]);
        let stored = fixture
            .store
            .load_block_translations(&fixture.job.id)
            .expect("stored blocks should load");
        assert!(
            stored
                .iter()
                .all(|block| block.text.as_str() != "stale cached text")
        );
        assert!(
            stored
                .iter()
                .any(|block| block.text.starts_with("[Italian]"))
        );
    }

    #[tokio::test]
    async fn resume_skips_needs_review_by_default() {
        let mut settings = TranslationProfile::V1Fast.resolve();
        settings.batch.enabled = false;
        let mut fixture = resume_fixture(settings, 2);
        for segment in &fixture.segments {
            save_needs_review(&fixture.store, &fixture.job.id, segment);
        }

        let events = run_fixture(&mut fixture)
            .await
            .expect("resume should succeed without retrying needs-review segments");

        assert!(segment_finished_ids(&events).is_empty());
        let summary = fixture
            .store
            .summary(&fixture.job.id)
            .expect("summary should load")
            .expect("job should exist");
        assert_eq!(summary.needs_review, 2);
        assert_eq!(summary.failed, 0);
    }

    #[tokio::test]
    async fn resume_rebuilds_output_from_stored_and_fresh_translations_in_original_order() {
        let segments = vec![
            test_segment("seg_a", 0),
            test_segment("seg_b", 1),
            test_segment("seg_c", 2),
        ];
        let stored = vec![
            StoredBlockTranslation {
                segment_id: "seg_c".to_string(),
                block_id: "b_000002".to_string(),
                text: "stored c".to_string(),
            },
            StoredBlockTranslation {
                segment_id: "seg_a".to_string(),
                block_id: "b_000000".to_string(),
                text: "stored a".to_string(),
            },
        ];
        let fresh = vec![bookforge_llm::SegmentTranslation {
            segment_id: SegmentId("seg_b".to_string()),
            ordinal: 1,
            block_ids: vec![BlockId("b_000001".to_string())],
            blocks: vec![BlockTranslation {
                block_id: BlockId("b_000001".to_string()),
                text: "fresh b".to_string(),
            }],
            checksum: "checksum_1".to_string(),
            status: SegmentStatus::Succeeded,
            template: "mock".to_string(),
            error: None,
            input_tokens: None,
            input_cached_tokens: None,
            output_tokens: None,
            tokens_estimated: false,
            findings: Vec::new(),
        }];

        let rebuilt = rebuild_block_translations(&segments, &stored, &fresh);
        let texts = rebuilt
            .iter()
            .map(|block| block.text.as_str())
            .collect::<Vec<_>>();

        assert_eq!(texts, vec!["stored a", "fresh b", "stored c"]);
    }

    #[tokio::test]
    async fn resume_errors_on_cache_namespace_mismatch() {
        let mut settings = TranslationProfile::V1Fast.resolve();
        settings.batch.enabled = false;
        let mut fixture = resume_fixture(settings, 1);
        fixture.snapshot.cache_namespace = "wrong-cache-namespace".to_string();

        let error = run_fixture(&mut fixture)
            .await
            .expect_err("cache namespace mismatch should fail");

        assert!(
            error
                .to_string()
                .contains("resume cache namespace mismatch")
        );
    }

    #[tokio::test]
    async fn resume_accepts_legacy_v1_snapshot_without_glossary_metadata() {
        let mut settings = TranslationProfile::V1Fast.resolve();
        settings.batch.enabled = false;
        let mut fixture = resume_fixture(settings.clone(), 1);
        fixture.snapshot.glossary_fingerprint.clear();
        fixture.snapshot.glossary_terms.clear();
        fixture.snapshot.cache_namespace = compute_cache_namespace_v1(
            settings.segmentation.max_segment_tokens,
            settings.segmentation.context_tokens,
            settings.profile.namespace_str(),
            settings.batch.enabled,
            fixture.snapshot.prompt_version.as_str(),
        );
        fixture
            .store
            .update_job_config_snapshot(&fixture.job.id, &fixture.snapshot)
            .expect("legacy snapshot should persist");

        let events = run_fixture(&mut fixture)
            .await
            .expect("legacy v1 snapshot should resume");

        assert!(!segment_finished_ids(&events).is_empty());
    }

    #[tokio::test]
    async fn resume_errors_when_rebuilt_segments_do_not_match_stored_pending_ids() {
        let segments = vec![test_segment("seg_a", 0)];
        let pending_ids = vec!["seg_missing".to_string()];

        let error = select_pending_segments(&segments, &pending_ids)
            .expect_err("missing pending IDs should fail");

        assert!(
            error
                .to_string()
                .contains("segment IDs that no longer exist")
        );
        assert!(error.to_string().contains("seg_missing"));
    }

    #[test]
    fn job_input_resolution_preserves_snapshot_precedence_and_legacy_errors() {
        let mut fixture = resume_fixture(TranslationProfile::V1Fast.resolve(), 1);
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("original.epub");
        let stored = dir.path().join("stored.epub");
        let configured = dir.path().join("configured.epub");
        for path in [&original, &stored, &configured] {
            std::fs::write(path, "fixture").unwrap();
        }
        fixture.snapshot.input_path = original.clone();
        assert_eq!(
            resolve_job_input(&fixture.job, &fixture.snapshot).unwrap(),
            original
        );
        fixture.job.input_snapshot_path = Some(stored.clone());
        assert_eq!(
            resolve_job_input(&fixture.job, &fixture.snapshot).unwrap(),
            stored
        );
        fixture.snapshot.input_snapshot_path = Some(configured.clone());
        assert_eq!(
            resolve_job_input(&fixture.job, &fixture.snapshot).unwrap(),
            configured
        );
        std::fs::remove_file(&configured).unwrap();
        assert!(
            resolve_job_input(&fixture.job, &fixture.snapshot)
                .unwrap_err()
                .to_string()
                .contains("input snapshot is missing")
        );
        fixture.snapshot.input_snapshot_path = None;
        std::fs::remove_file(&stored).unwrap();
        assert!(resolve_job_input(&fixture.job, &fixture.snapshot).is_err());
        fixture.job.input_snapshot_path = None;
        std::fs::remove_file(&original).unwrap();
        assert!(
            resolve_job_input(&fixture.job, &fixture.snapshot)
                .unwrap_err()
                .to_string()
                .contains("original input path no longer exists")
        );
    }

    #[tokio::test]
    async fn resume_revalidates_saved_copies_and_keeps_explicit_acceptance() {
        let chapter = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<head><title>Endnotes</title></head>
<body>
<h1>Endnotes</h1>
<p>This deliberately long English paragraph explains why the narrator returned to the village after the winter. The people had kept every letter and remembered the promises, but their circumstances had changed and they needed an answer before the next meeting.</p>
</body>
</html>"#;
        let mut settings = TranslationProfile::V1Fast.resolve();
        settings.batch.enabled = false;
        settings.segmentation.max_segment_tokens = 1000;
        settings.double_check.mode = DoubleCheckMode::Off;
        let mut fixture = resume_fixture_with_chapter(settings, 1, chapter, |segment| {
            segment
                .source
                .blocks
                .iter()
                .any(|block| block.text.chars().count() > 120)
        });
        let segment = fixture.segments[0].clone();
        let blocks = segment
            .source
            .blocks
            .iter()
            .map(|block| BlockTranslation {
                block_id: block.block_id.clone(),
                text: block.text.clone(),
            })
            .collect::<Vec<_>>();
        fixture
            .store
            .save_translation(SaveTranslation {
                job_id: &fixture.job.id,
                segment_id: &segment.id.0,
                translated_text: &segment.source.text,
                blocks: &blocks,
                // Revalidation follows the saved provider, which can differ
                // from the job's primary provider after a fallback.
                provider: "deepseek",
                model: "legacy-provider",
                prompt_version: "v1",
                input_tokens: Some(91),
                input_cached_tokens: Some(17),
                output_tokens: Some(61),
                tokens_estimated: false,
            })
            .unwrap();

        run_fixture(&mut fixture).await.unwrap();

        let records = fixture
            .store
            .load_terminal_segment_translations(&fixture.job.id)
            .unwrap();
        assert_eq!(records[0].status, "needs_review");
        assert_eq!(records[0].blocks, blocks);
        assert_eq!(records[0].provider, "deepseek");
        let summary = fixture.store.summary(&fixture.job.id).unwrap().unwrap();
        assert_eq!(
            (
                summary.needs_review,
                summary.input_tokens,
                summary.output_tokens
            ),
            (1, 91, 61)
        );
        assert_eq!(
            fixture.store.segment_records(&fixture.job.id).unwrap()[0].attempts,
            1,
            "a flagged copy is kept for review, not retranslated"
        );
        assert!(fixture.snapshot.output_path.exists());

        fixture
            .store
            .save_manual_correction(SaveManualCorrection {
                job_id: &fixture.job.id,
                segment_id: &segment.id.0,
                translated_text: &segment.source.text,
                blocks: &blocks,
            })
            .unwrap();
        let accepted = fixture
            .store
            .load_terminal_segment_translations(&fixture.job.id)
            .unwrap();
        // Every segment is human-accepted, so optional double-check must not
        // build its provider. This one points at a closed local port with no
        // API key: reaching it at all would fail the resume.
        let double_check = &mut fixture.snapshot.settings.double_check;
        double_check.mode = DoubleCheckMode::Full;
        double_check.provider = Some("openai-compatible".into());
        double_check.base_url = Some("http://127.0.0.1:9/v1".into());
        double_check.api_key_env = Some("BOOKFORGE_TEST_UNSET_DOUBLE_CHECK_KEY".into());
        fixture.snapshot.finalize.double_check_complete = false;
        run_fixture(&mut fixture).await.unwrap();
        assert_eq!(
            fixture
                .store
                .load_terminal_segment_translations(&fixture.job.id)
                .unwrap(),
            accepted
        );
        assert!(accepted[0].human_corrected);
        assert_eq!(accepted[0].status, "succeeded");
    }

    #[test]
    fn plain_resume_refuses_a_fresh_worker_lease() {
        let job_id = format!("resume-active-lease-{}-{}", std::process::id(), now_ms());
        let path = crate::control::runtime_path_for_job(&job_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        assert!(
            ensure_no_live_worker(&job_id).is_ok(),
            "no lease, no worker"
        );
        let lease = crate::control::RuntimeLease {
            schema_version: 1,
            instance_id: "active-test-worker".to_string(),
            pid: std::process::id(),
            process_started_at_ms: now_ms(),
            heartbeat_at_ms: now_ms(),
            last_loaded_revision: 0,
            last_applied_revision: 0,
        };
        std::fs::write(&path, serde_json::to_vec(&lease).unwrap()).unwrap();
        let error = ensure_no_live_worker(&job_id).expect_err("a live worker must block");
        assert!(error.to_string().contains("already has an active worker"));
        assert!(error.to_string().contains("--force"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn resume_keeps_blockless_malformed_responses_needing_review() {
        let mut settings = TranslationProfile::V1Fast.resolve();
        settings.provider.validation_max_attempts = 1;
        let mut fixture = resume_fixture(settings, 1);
        fixture.snapshot.model = "mock-malformed-json".to_string();

        run_fixture(&mut fixture)
            .await
            .expect("resume should finish");
        let status = |fixture: &ResumeFixture| {
            fixture
                .store
                .get_job(&fixture.job.id)
                .unwrap()
                .unwrap()
                .status
        };
        let summary = fixture.store.summary(&fixture.job.id).unwrap().unwrap();
        assert_eq!(summary.succeeded, 0);
        assert_eq!(summary.needs_review + summary.failed, 1);
        assert_eq!(status(&fixture), "needs_review");

        // Resuming the exhausted job again must not turn source fallbacks green.
        run_fixture(&mut fixture)
            .await
            .expect("repeat resume should finish");
        assert_eq!(status(&fixture), "needs_review");
    }

    #[test]
    fn finalization_retains_persisted_failure_without_translation_blocks() {
        let fixture = resume_fixture(TranslationProfile::V1Fast.resolve(), 1);
        fixture
            .store
            .mark_segment_failed(
                &fixture.job.id,
                &fixture.segments[0].id.0,
                "provider unavailable",
            )
            .unwrap();

        assert!(mark_job_finished(&fixture.store, &fixture.job.id, &[]).unwrap());
        assert_eq!(
            fixture
                .store
                .get_job(&fixture.job.id)
                .unwrap()
                .unwrap()
                .status,
            "needs_review"
        );
        let records = fixture.store.segment_records(&fixture.job.id).unwrap();
        let restored = rebuild_segment_translations(&fixture.segments, &[], &records);
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].status, SegmentStatus::Failed);
        assert!(restored[0].blocks.is_empty());
        assert_eq!(restored[0].error.as_deref(), Some("provider unavailable"));
    }

    /// Cancels the caller token at the first request (or the first queued
    /// checkpoint), simulating Ctrl+C in the middle of a resumed run.
    struct CancellingSink {
        token: tokio_util::sync::CancellationToken,
        events: Arc<Mutex<Vec<ProgressEvent>>>,
        after_checkpoint: bool,
    }

    impl ProgressSink for CancellingSink {
        fn emit(&self, event: ProgressEvent) {
            let should_cancel = if self.after_checkpoint {
                matches!(event, ProgressEvent::CheckpointQueued { .. })
            } else {
                matches!(event, ProgressEvent::RequestStarted { .. })
            };
            self.events.lock().unwrap().push(event);
            if should_cancel {
                self.token.cancel();
            }
        }
    }

    fn job_status(fixture: &ResumeFixture) -> String {
        fixture
            .store
            .get_job(&fixture.job.id)
            .unwrap()
            .unwrap()
            .status
    }

    #[tokio::test]
    async fn resume_cancellation_stops_dispatch_and_drains_checkpoints() {
        for after_checkpoint in [false, true] {
            let mut settings = TranslationProfile::V1Fast.resolve();
            settings.scheduler.concurrency = 1;
            settings.batch.max_items = 1;
            // One block per item keeps this fixture independent of the mock
            // batch parser's malformed-response repair.
            settings.segmentation.max_segment_tokens = 1;
            let mut fixture = resume_fixture(settings, 3);
            let token = tokio_util::sync::CancellationToken::new();
            let events = Arc::new(Mutex::new(Vec::new()));
            let sink = Arc::new(CancellingSink {
                token: token.clone(),
                events: events.clone(),
                after_checkpoint,
            });

            tokio::time::timeout(
                Duration::from_secs(20),
                run_fixture_with_progress(&mut fixture, None, sink, token.clone()),
            )
            .await
            .expect("cancellation must finish promptly")
            .expect("checkpoint drain should succeed");

            assert!(token.is_cancelled());
            assert_eq!(job_status(&fixture), "interrupted");
            assert!(
                !fixture.snapshot.output_path.exists(),
                "interruption must not publish a source-fallback book"
            );
            {
                let recorded = events.lock().unwrap();
                let requests = recorded
                    .iter()
                    .filter(|event| matches!(event, ProgressEvent::RequestStarted { .. }))
                    .count();
                assert!(
                    requests < fixture.segments.len(),
                    "cancellation must stop new dispatch: {requests} requests"
                );
                let queued = recorded
                    .iter()
                    .filter(|event| matches!(event, ProgressEvent::CheckpointQueued { .. }))
                    .count();
                let flushed = recorded
                    .iter()
                    .filter(|event| matches!(event, ProgressEvent::CheckpointFlushed { .. }))
                    .count();
                assert_eq!(
                    queued, flushed,
                    "every completed response must be checkpointed before return"
                );
                assert!(
                    !recorded
                        .iter()
                        .any(|event| matches!(event, ProgressEvent::TranslationFinished { .. })),
                    "an interrupted run must not signal completion"
                );
                if after_checkpoint {
                    assert!(flushed > 0);
                    assert!(
                        !fixture
                            .store
                            .load_block_translations(&fixture.job.id)
                            .unwrap()
                            .is_empty()
                    );
                }
            }

            run_fixture(&mut fixture)
                .await
                .expect("interrupted job should resume");
            assert_eq!(
                job_status(&fixture),
                "succeeded",
                "remaining records: {:?}",
                fixture.store.segment_records(&fixture.job.id).unwrap()
            );
            assert!(fixture.snapshot.output_path.exists());
        }
    }

    #[tokio::test]
    async fn resume_cancelled_before_dispatch_preserves_stopped_precedence() {
        for stopped in [false, true] {
            let mut fixture = resume_fixture(TranslationProfile::V1Fast.resolve(), 1);
            if stopped {
                fixture.store.mark_job_stopped(&fixture.job.id).unwrap();
            }
            let token = tokio_util::sync::CancellationToken::new();
            token.cancel();
            let events = Arc::new(Mutex::new(Vec::new()));
            let sink = Arc::new(RecordingSink {
                events: events.clone(),
            });
            run_fixture_with_progress(&mut fixture, None, sink, token)
                .await
                .unwrap();
            assert_eq!(
                job_status(&fixture),
                if stopped { "stopped" } else { "interrupted" }
            );
            assert!(events.lock().unwrap().is_empty());
            assert_eq!(
                fixture
                    .store
                    .resumable_segment_ids(&fixture.job.id)
                    .unwrap()
                    .len(),
                1
            );
        }
    }

    #[tokio::test]
    async fn resume_cancels_stalled_http_in_translation_and_finalization() {
        use tokio::{io::AsyncReadExt, net::TcpListener};

        for stage in ["translation", "batch", "qa", "fallback", "double_check"] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let token = tokio_util::sync::CancellationToken::new();
            let request_token = token.clone();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut first_byte = [0];
                stream.read_exact(&mut first_byte).await.unwrap();
                // Hold the accepted request open without answering: only the
                // caller's token can end it before the 60 s provider timeout.
                request_token.cancel();
                std::future::pending::<()>().await;
                drop(stream);
            });
            let mut settings = TranslationProfile::V1Fast.resolve();
            settings.batch.enabled = stage == "batch";
            settings.scheduler.concurrency = 1;
            settings.provider.timeout_seconds = 60;
            settings.provider.provider_max_attempts = 1;
            if stage == "double_check" {
                settings.double_check.mode = DoubleCheckMode::Full;
            }
            let mut fixture = resume_fixture(settings, 1);
            fixture.job.provider = "openai-compatible".to_string();
            fixture.snapshot.provider = "openai-compatible".to_string();
            fixture.snapshot.model = "local-cancellation-test".to_string();
            fixture.snapshot.base_url = Some(base_url.clone());
            fixture.snapshot.api_key_env = Some("OLLAMA_API_KEY".to_string());
            match stage {
                "qa" | "double_check" => {
                    save_succeeded(
                        &fixture.store,
                        &fixture.job.id,
                        &fixture.segments[0],
                        "stored",
                    );
                    if stage == "qa" {
                        fixture.snapshot.qa_mode = "all".to_string();
                    }
                }
                "fallback" => {
                    save_needs_review(&fixture.store, &fixture.job.id, &fixture.segments[0]);
                    fixture.snapshot.fallback =
                        Some(bookforge_core::run_snapshot::FallbackRunConfigSnapshot {
                            provider: "openai-compatible".to_string(),
                            model: "local-cancellation-test".to_string(),
                            base_url: Some(base_url),
                            api_key_env: Some("OLLAMA_API_KEY".to_string()),
                            scope: FallbackScope::NeedsReview,
                        });
                }
                _ => {}
            }
            let events = Arc::new(Mutex::new(Vec::new()));
            let sink = Arc::new(RecordingSink {
                events: events.clone(),
            });
            let outcome = tokio::time::timeout(
                Duration::from_secs(20),
                run_fixture_with_progress(&mut fixture, None, sink, token.clone()),
            )
            .await;
            server.abort();
            outcome
                .expect("caller cancellation should abort stalled HTTP promptly")
                .unwrap_or_else(|error| panic!("cancelled {stage} failed to finalize: {error}"));
            assert!(
                token.is_cancelled(),
                "{stage} must make the loopback request"
            );
            assert_eq!(job_status(&fixture), "interrupted", "stage {stage}");
            assert!(
                !fixture.snapshot.output_path.exists(),
                "stage {stage} must not publish after cancellation"
            );
            assert!(
                !fixture.snapshot.finalize.double_check_complete,
                "cancelled double-check must remain resumable"
            );
            assert!(
                !events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|event| matches!(event, ProgressEvent::TranslationFinished { .. })),
                "stage {stage} must not signal completion"
            );
        }
    }

    #[test]
    fn preflight_clears_only_the_original_control_revision() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control");
        let write_revision = |command, seconds| {
            bookforge_core::write_control_file(&path, command).unwrap();
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(UNIX_EPOCH + Duration::from_secs(seconds))
                .unwrap();
        };

        write_revision(ControlCommand::Stop, 60);
        let original = control_revision(&path).unwrap();
        clear_preflight_control(&path, original).unwrap();
        assert!(!path.exists());

        // Even a repeated Stop is a new request when written during preflight.
        write_revision(ControlCommand::Stop, 60);
        let original = control_revision(&path).unwrap();
        write_revision(ControlCommand::Stop, 120);
        clear_preflight_control(&path, original).unwrap();
        assert_eq!(
            bookforge_core::read_control_file(&path).unwrap(),
            ControlCommand::Stop
        );

        let original = control_revision(&path).unwrap();
        write_revision(ControlCommand::Pause, 180);
        clear_preflight_control(&path, original).unwrap();
        assert_eq!(
            bookforge_core::read_control_file(&path).unwrap(),
            ControlCommand::Pause
        );

        bookforge_core::clear_control_file(&path).unwrap();
        let absent = control_revision(&path).unwrap();
        write_revision(ControlCommand::Stop, 240);
        clear_preflight_control(&path, absent).unwrap();
        assert_eq!(
            bookforge_core::read_control_file(&path).unwrap(),
            ControlCommand::Stop
        );
    }

    #[tokio::test]
    async fn rejected_resume_keeps_the_job_state_paths_and_pending_control() {
        let settings = TranslationProfile::V1Fast.resolve();
        let mut fixture = resume_fixture(settings, 2);
        save_succeeded(
            &fixture.store,
            &fixture.job.id,
            &fixture.segments[0],
            "stored",
        );
        fixture.store.mark_job_stopped(&fixture.job.id).unwrap();
        fixture.job = fixture.store.get_job(&fixture.job.id).unwrap().unwrap();
        fixture.snapshot.cache_namespace = "incompatible-namespace".to_string();
        fixture
            .store
            .update_job_config_snapshot(&fixture.job.id, &fixture.snapshot)
            .unwrap();
        let control = bookforge_core::control_path_for_job(&fixture.job.id);
        bookforge_core::write_control_file(&control, ControlCommand::Stop).unwrap();
        let before = serde_json::to_value(&fixture.snapshot).unwrap();
        let blocks = fixture
            .store
            .load_block_translations(&fixture.job.id)
            .unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::new(RecordingSink {
            events: events.clone(),
        });
        let mut args = resume_args(&fixture.job.id);
        let tempdir = tempfile::tempdir().unwrap();
        args.output = Some(tempdir.path().join("elsewhere.epub"));
        args.progress_jsonl = Some(tempdir.path().join("elsewhere.jsonl"));
        let run_store = JobStore::open(fixture.store.path()).unwrap();
        let error = run_inner(
            &args,
            &run_store,
            fixture.job.clone(),
            &mut fixture.snapshot,
            None,
            sink,
            tokio_util::sync::CancellationToken::new(),
            None,
        )
        .await
        .expect_err("an incompatible checkpoint contract must not resume");

        assert!(
            error
                .to_string()
                .contains("resume cache namespace mismatch")
        );
        assert!(!events.lock().unwrap().iter().any(|event| matches!(
            event,
            ProgressEvent::RequestStarted { .. } | ProgressEvent::SegmentFinished { .. }
        )));
        assert_eq!(
            fixture
                .store
                .load_block_translations(&fixture.job.id)
                .unwrap(),
            blocks
        );
        let saved = fixture
            .store
            .load_job_config_snapshot(&fixture.job.id)
            .unwrap()
            .unwrap();
        assert_eq!(serde_json::to_value(&saved).unwrap(), before);
        let job = fixture.store.get_job(&fixture.job.id).unwrap().unwrap();
        assert_eq!(job.status, "stopped");
        assert_eq!(job.output_path, fixture.snapshot.output_path);
        assert_eq!(
            bookforge_core::read_control_file(&control).unwrap(),
            ControlCommand::Stop,
            "the pending command stays until a resume is accepted"
        );
        assert!(!fixture.snapshot.output_path.exists());
        bookforge_core::clear_control_file(&control).unwrap();
    }

    #[tokio::test]
    async fn accepted_resume_clears_the_stale_control_command() {
        let mut fixture = resume_fixture(TranslationProfile::V1Fast.resolve(), 1);
        fixture.store.mark_job_stopped(&fixture.job.id).unwrap();
        fixture.job = fixture.store.get_job(&fixture.job.id).unwrap().unwrap();
        let control = bookforge_core::control_path_for_job(&fixture.job.id);
        bookforge_core::write_control_file(&control, ControlCommand::Stop).unwrap();

        run_fixture(&mut fixture)
            .await
            .expect("a compatible resume should run");
        assert!(!control.exists());
        assert_eq!(job_status(&fixture), "succeeded");
    }

    fn resume_fixture(settings: ResolvedRunSettings, segment_count: usize) -> ResumeFixture {
        resume_fixture_with_chapter(settings, segment_count, RESUME_CHAPTER_ONE, |_| true)
    }

    /// [`resume_fixture`] with a custom first chapter, keeping the first
    /// `segment_count` segments that `select` accepts.
    fn resume_fixture_with_chapter(
        settings: ResolvedRunSettings,
        segment_count: usize,
        chapter_one: &str,
        select: impl Fn(&Segment) -> bool,
    ) -> ResumeFixture {
        let tempdir = tempfile::tempdir().expect("tempdir should be created");
        let input = fixture_input_with_chapter(chapter_one);
        let output = tempdir.path().join("translated.epub");
        let events = tempdir.path().join("events.jsonl");
        let store = JobStore::open(tempdir.path().join("jobs.sqlite")).expect("store should open");
        let job = store
            .create_job(CreateJob {
                input: &input,
                output: &output,
                source_lang: Some("English"),
                target_lang: "Italian",
                provider: "mock",
                model: "mock-prefix-target",
                base_url: None,
                api_key_env: None,
                book_id: None,
                series_id: None,
            })
            .expect("job should be created");
        let book = read_epub(&input).expect("fixture EPUB should read");
        let all_segments =
            build_segments(&book, &settings.segmentation).expect("segments should build");
        assert!(
            all_segments.len() >= segment_count,
            "fixture should have enough segments"
        );
        let segments = all_segments
            .into_iter()
            .filter(|segment| select(segment))
            .take(segment_count)
            .collect::<Vec<_>>();
        let prompt_version = "v1";
        let glossary_fingerprint = crate::commands::translate::glossary_fingerprint(
            bookforge_core::GlossaryFormat::Json,
            800,
            None,
            &[],
        );
        let cache_namespace = compute_cache_namespace(
            settings.segmentation.max_segment_tokens,
            settings.segmentation.context_tokens,
            settings.profile.namespace_str(),
            settings.batch.enabled,
            prompt_version,
            &glossary_fingerprint,
            "",
            "",
        );
        store
            .insert_segments(
                &job.id,
                &segments,
                prompt_version,
                "mock",
                "mock-prefix-target",
                &cache_namespace,
            )
            .expect("segments should insert");
        let snapshot = RunConfigSnapshot {
            input_path: input,
            input_snapshot_path: None,
            input_sha256: None,
            output_path: output,
            events_path: Some(events),
            report_json_path: None,
            report_markdown_path: None,
            source_language: Some("English".to_string()),
            target_language: "Italian".to_string(),
            creator: None,
            provider: "mock".to_string(),
            model: "mock-prefix-target".to_string(),
            base_url: None,
            api_key_env: None,
            profile: settings.profile,
            provider_preset: None,
            prompt_version: prompt_version.to_string(),
            cache_namespace,
            book_id: None,
            series_id: None,
            glossary_budget_tokens: 800,
            glossary_format: bookforge_core::GlossaryFormat::Json,
            prompt_extra: None,
            glossary_fingerprint,
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

        ResumeFixture {
            _tempdir: tempdir,
            store,
            job,
            snapshot,
            segments,
        }
    }

    async fn run_fixture(fixture: &mut ResumeFixture) -> Result<Vec<ProgressEvent>> {
        run_fixture_with_overrides(fixture, None).await
    }

    async fn run_fixture_with_overrides(
        fixture: &mut ResumeFixture,
        overrides: Option<reconfigure::RunConfigOverrides>,
    ) -> Result<Vec<ProgressEvent>> {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::new(RecordingSink {
            events: events.clone(),
        });
        run_fixture_with_progress(
            fixture,
            overrides,
            sink,
            tokio_util::sync::CancellationToken::new(),
        )
        .await?;
        Ok(events.lock().expect("events lock").clone())
    }

    async fn run_fixture_with_progress(
        fixture: &mut ResumeFixture,
        overrides: Option<reconfigure::RunConfigOverrides>,
        progress: Arc<dyn ProgressSink>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<()> {
        let run_store = JobStore::open(fixture.store.path()).expect("store should reopen");
        let args = resume_args(&fixture.job.id);
        run_inner(
            &args,
            &run_store,
            fixture.job.clone(),
            &mut fixture.snapshot,
            overrides,
            progress,
            cancel_token,
            None,
        )
        .await
    }

    fn resume_args(job_id: &str) -> ResumeArgs {
        ResumeArgs {
            job_id: job_id.to_string(),
            concurrency: None,
            max_attempts: None,
            provider_max_attempts: None,
            validation_max_attempts: None,
            qa: None,
            timeout_seconds: None,
            max_output_tokens: None,
            ui: None,
            progress_jsonl: None,
            output: None,
            no_thinking: false,
            validate_output: false,
            strict_epubcheck: false,
            qa_concurrency: None,
            qa_batch_target_tokens: None,
            qa_max_output_tokens: None,
            qa_model: None,
            qa_provider: None,
            qa_base_url: None,
            qa_api_key_env: None,
            double_check: None,
            double_check_model: None,
            double_check_provider: None,
            double_check_base_url: None,
            double_check_api_key_env: None,
            double_check_concurrency: None,
            fallback_provider: None,
            fallback_model: None,
            fallback_base_url: None,
            fallback_api_key_env: None,
            fallback_only: None,
            force: false,
        }
    }

    fn write_overrides_sidecar(
        job_id: &str,
        overrides: &reconfigure::RunConfigOverrides,
    ) -> PathBuf {
        let path = reconfigure::overrides_path_for_job(job_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("sidecar parent should exist");
        }
        let json = serde_json::to_string_pretty(overrides).expect("overrides should serialize");
        std::fs::write(&path, format!("{json}\n")).expect("overrides sidecar should write");
        path
    }

    fn runtime_config_event(events: &[ProgressEvent]) -> RuntimeConfigEvent<'_> {
        events
            .iter()
            .find_map(|event| {
                if let ProgressEvent::RuntimeConfigResolved {
                    profile,
                    provider_max_attempts,
                    batch_target_tokens,
                    batch_max_items,
                    adaptive_batch_sizing,
                    adaptive_concurrency,
                    batch_max_output_tokens,
                    compact_prompts,
                    json_mode,
                    ..
                } = event
                {
                    Some(RuntimeConfigEvent {
                        profile,
                        provider_max_attempts: *provider_max_attempts,
                        batch_target_tokens: *batch_target_tokens,
                        batch_max_items: *batch_max_items,
                        adaptive_batch_sizing: *adaptive_batch_sizing,
                        adaptive_concurrency: *adaptive_concurrency,
                        batch_max_output_tokens: *batch_max_output_tokens,
                        compact_prompts: *compact_prompts,
                        json_mode,
                    })
                } else {
                    None
                }
            })
            .expect("runtime config event should be emitted")
    }

    struct RuntimeConfigEvent<'a> {
        profile: &'a str,
        provider_max_attempts: usize,
        batch_target_tokens: usize,
        batch_max_items: usize,
        adaptive_batch_sizing: bool,
        adaptive_concurrency: bool,
        batch_max_output_tokens: Option<u32>,
        compact_prompts: bool,
        json_mode: &'a str,
    }

    fn segment_finished_ids(events: &[ProgressEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| {
                if let ProgressEvent::SegmentFinished { segment_id, .. } = event {
                    Some(segment_id.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    fn save_succeeded(store: &JobStore, job_id: &str, segment: &Segment, prefix: &str) {
        let blocks = translated_blocks(segment, prefix);
        let translated_text = blocks
            .iter()
            .map(|block| block.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        store
            .save_translation(SaveTranslation {
                job_id,
                segment_id: &segment.id.0,
                translated_text: &translated_text,
                blocks: &blocks,
                provider: "mock",
                model: "mock-prefix-target",
                prompt_version: "v1",
                input_tokens: Some(1),
                input_cached_tokens: Some(0),
                output_tokens: Some(1),
                tokens_estimated: false,
            })
            .expect("translation should save");
    }

    fn save_needs_review(store: &JobStore, job_id: &str, segment: &Segment) {
        let blocks = translated_blocks(segment, "review");
        let preserved_text = blocks
            .iter()
            .map(|block| block.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        store
            .save_needs_review(SaveNeedsReview {
                job_id,
                segment_id: &segment.id.0,
                preserved_text: &preserved_text,
                blocks: &blocks,
                provider: "mock",
                model: "mock-prefix-target",
                prompt_version: "v1",
                error: "manual review",
                input_tokens: Some(1),
                input_cached_tokens: Some(0),
                output_tokens: Some(1),
                tokens_estimated: false,
            })
            .expect("needs-review translation should save");
    }

    fn translated_blocks(segment: &Segment, prefix: &str) -> Vec<BlockTranslation> {
        segment
            .source
            .blocks
            .iter()
            .map(|block| BlockTranslation {
                block_id: block.block_id.clone(),
                text: format!("{prefix} {}", block.block_id.0),
            })
            .collect()
    }

    // Builds a synthetic EPUB so resume tests exercise the EPUB path in CI
    // without relying on local, gitignored book fixtures.
    fn fixture_input() -> PathBuf {
        fixture_input_with_chapter(RESUME_CHAPTER_ONE)
    }

    fn fixture_input_with_chapter(chapter_one: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "bookforge-resume-fixture-{}-{nanos}.epub",
            std::process::id()
        ));
        build_resume_epub(&path, chapter_one);
        path
    }

    fn build_resume_epub(path: &Path, chapter_one: &str) {
        let file = std::fs::File::create(path).expect("fixture EPUB should be creatable");
        let mut zip = ZipWriter::new(file);
        let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

        zip.start_file("mimetype", stored).unwrap();
        zip.write_all(b"application/epub+zip").unwrap();
        zip.start_file("META-INF/container.xml", deflated).unwrap();
        zip.write_all(RESUME_CONTAINER_XML.as_bytes()).unwrap();
        zip.start_file("content.opf", deflated).unwrap();
        zip.write_all(RESUME_OPF.as_bytes()).unwrap();
        zip.start_file("chapter1.xhtml", deflated).unwrap();
        zip.write_all(chapter_one.as_bytes()).unwrap();
        zip.start_file("chapter2.xhtml", deflated).unwrap();
        zip.write_all(RESUME_CHAPTER_TWO.as_bytes()).unwrap();
        zip.finish().unwrap();
    }

    const RESUME_CONTAINER_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;

    const RESUME_OPF: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="uid">resume-fixture</dc:identifier>
    <dc:title>Resume Fixture</dc:title>
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

    const RESUME_CHAPTER_ONE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<head><title>Resume Chapter One</title></head>
<body>
<h1>Resume Chapter One</h1>
<p>First resume paragraph with <em>inline emphasis</em> and a <a href="https://example.com">link</a>.</p>
<p>Second resume paragraph gives the fixture enough blocks for retry and cache tests.</p>
</body>
</html>"#;

    const RESUME_CHAPTER_TWO: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<head><title>Resume Chapter Two</title></head>
<body>
<h1>Resume Chapter Two</h1>
<p>Another section keeps segmentation boundaries stable across resume tests.</p>
</body>
</html>"#;

    fn test_segment(id: &str, ordinal: usize) -> Segment {
        let block_id = BlockId(format!("b_{ordinal:06}"));
        Segment {
            id: SegmentId(id.to_string()),
            section_id: SectionId("sec_000000".to_string()),
            ordinal,
            block_ids: vec![block_id.clone()],
            source: SegmentSource {
                text: format!("Source {ordinal}"),
                blocks: vec![SegmentBlock {
                    block_id,
                    kind: "paragraph".to_string(),
                    text: format!("Source {ordinal}"),
                    text_runs: vec![SegmentTextRun {
                        id: format!("r{ordinal}"),
                        text: format!("Source {ordinal}"),
                    }],
                    protected_spans: Vec::new(),
                }],
                token_estimate: 2,
            },
            context: SegmentContext::default(),
            metadata: SegmentMetadata::default(),
            constraints: SegmentConstraints::default(),
            checksum: format!("checksum_{ordinal}"),
        }
    }
}
