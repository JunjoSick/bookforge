use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::Result;
use bookforge_core::{
    ControlCommand, ProgressEvent, ProgressSink, ResolvedRunSettings, RunConfigSnapshot,
    config::DoubleCheckMode,
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
    report::{ReportInput, TranslationQaInput, write_report},
};

use super::translate::{
    CacheContext, CheckpointRunContext, FallbackPassConfig, ProgressRequestProvider,
    apply_cached_translations, apply_double_check_corrections, job_was_stopped, mark_job_finished,
    mock_mode, persist_corrected_translations, print_stopped_resume_hint, qa_reviews_for_mode,
    run_checkpointed_translation, run_fallback_pass,
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

    #[arg(long, default_value_t = false)]
    pub no_thinking: bool,

    #[arg(
        long,
        default_value_t = false,
        help = "Relaunch a paused job; only use if the paused process is gone because this can double-run the job"
    )]
    pub force: bool,
}

pub async fn run(args: ResumeArgs) -> Result<()> {
    let store = JobStore::open_default()?;
    let Some(job) = store.get_job(&args.job_id)? else {
        anyhow::bail!("job '{}' was not found", args.job_id);
    };
    if job.status == "paused" && !args.force {
        return live_fast_resume_paused_job(&store, &args).await;
    }
    crate::control::clear_job_control(&args.job_id)?;
    if (job.status == "paused" && args.force) || job.status == "stopped" {
        store.mark_job_running_for_resume(&args.job_id)?;
    }
    let mut snapshot = load_resume_snapshot(&store, &args.job_id)?;
    let overrides = load_resume_overrides_for_status(&args.job_id, &job.status)?;

    let progress_jsonl = args
        .progress_jsonl
        .clone()
        .or_else(|| snapshot.events_path.clone());
    let reporter = crate::progress::ProgressReporter::spawn_with_append(
        args.ui.unwrap_or(crate::progress::UiMode::Auto),
        progress_jsonl,
        true,
    );
    let progress = reporter.sink();

    let run_result = run_inner(args, store, job, &mut snapshot, overrides, progress).await;
    finalize_reporter(run_result, reporter).await
}

async fn live_fast_resume_paused_job(store: &JobStore, args: &ResumeArgs) -> Result<()> {
    if reconfigure::load_overrides_for_job(&args.job_id)?.is_some() {
        anyhow::bail!(
            "job '{}' has pending reconfigure overrides that a live fast-resume cannot apply. {}",
            args.job_id,
            reconfigure::apply_instructions(&args.job_id)
        );
    }
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
        "paused" | "stopped" => reconfigure::load_overrides_for_job(job_id),
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

async fn finalize_reporter<T>(
    result: Result<T, anyhow::Error>,
    reporter: crate::progress::ProgressReporter,
) -> Result<T> {
    let reporter_result = reporter.shutdown().await;
    match (result, reporter_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(e)) => Err(e),
        (Err(e), Ok(())) => Err(e),
        (Err(main_err), Err(progress_err)) => Err(anyhow::anyhow!(
            "{main_err}; additionally progress reporter failed: {progress_err}"
        )),
    }
}

async fn run_inner(
    args: ResumeArgs,
    store: JobStore,
    job: JobRecord,
    snapshot: &mut RunConfigSnapshot,
    overrides: Option<reconfigure::RunConfigOverrides>,
    progress: Arc<dyn ProgressSink>,
) -> Result<()> {
    let started = std::time::Instant::now();
    progress.emit(ProgressEvent::StageStarted {
        stage: "resume".to_string(),
        timestamp_ms: now_ms(),
    });
    let input = resolve_resume_input(&job, snapshot)?;
    let output = args
        .output
        .clone()
        .unwrap_or_else(|| snapshot.output_path.clone());
    if args.output.is_some() {
        store.update_job_output_path(&job.id, &output)?;
        snapshot.output_path = output.clone();
    }
    if let Some(path) = args.progress_jsonl.clone() {
        store.update_job_event_path(&job.id, &path)?;
        snapshot.events_path = Some(path);
    }
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
        .unwrap_or(QaMode::Off);
    let validate_output = overrides
        .as_ref()
        .and_then(|overrides| overrides.validate_output)
        .unwrap_or(false);
    if let Some(overrides) = overrides.as_ref() {
        reconfigure::apply_overrides_to_settings(&mut settings, overrides);
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
    let context_cfg = snapshot_context_run_config(snapshot);
    let context_registry: Option<Arc<ContextRegistry>> = if context_cfg.enabled() {
        let registry = Arc::new(ContextRegistry::new(&segments));
        rehydrate_context_registry_from_store(&registry, &segments, &store, &job.id)?;
        Some(registry)
    } else {
        None
    };
    let pause_signal = bookforge_llm::PauseSignal::new();
    let stop_cancel_token = tokio_util::sync::CancellationToken::new();
    let _control_watcher = crate::control::ControlFileWatcher::spawn_with_stop_cancel(
        store.path().to_path_buf(),
        job.id.clone(),
        progress.clone(),
        pause_signal.clone(),
        stop_cancel_token.clone(),
    );
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
    };

    let cache_namespace = if legacy_cache_namespace {
        compute_cache_namespace_v1(
            settings.segmentation.max_segment_tokens,
            settings.segmentation.context_tokens,
            run_config.profile.namespace_str(),
            settings.batch.enabled,
            prompt_version,
        )
    } else {
        compute_cache_namespace(
            settings.segmentation.max_segment_tokens,
            settings.segmentation.context_tokens,
            run_config.profile.namespace_str(),
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
        &cacheable_pending_segments,
        CacheContext {
            store: &store,
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
                        store: &store,
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
                let provider_config = openai_compatible_config(&job, snapshot, &settings)?;
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
                        store: &store,
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
    if job_was_stopped(&store, &job.id)? {
        print_stopped_resume_hint(&job.id, print_stdout);
        return Ok(());
    }

    let mut stored_blocks = store.load_block_translations(&job.id)?;
    let mut segment_records = store.segment_records(&job.id)?;
    let mut translations =
        rebuild_segment_translations(&segments, &stored_blocks, &segment_records);
    let mut control_poller = crate::control::ControlFilePoller::new_with_stop_cancel(
        &store,
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
        print_stopped_resume_hint(&job.id, print_stdout);
        return Ok(());
    }
    let qa_reviews = qa_after_resume(
        &job,
        &segments,
        &translations,
        &run_config,
        snapshot,
        &settings,
        qa_mode,
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
        print_stopped_resume_hint(&job.id, print_stdout);
        return Ok(());
    }
    let fallback_config = FallbackPassConfig::from_snapshot(snapshot.fallback.as_ref());
    let fallback_translations = run_fallback_pass(
        &stop_cancel_token,
        fallback_config.as_ref(),
        &segments,
        translations,
        &store,
        &job.id,
        prompt_version,
        &settings,
        &run_config,
        Some(&mut control_poller),
        progress.clone(),
    )
    .await?;
    translations = fallback_translations;
    if matches!(
        control_poller
            .wait_until_running_or_stopped(&pause_signal)
            .await?,
        bookforge_llm::PauseState::Stopped
    ) {
        print_stopped_resume_hint(&job.id, print_stdout);
        return Ok(());
    }
    if settings.double_check.mode != DoubleCheckMode::Off
        && !snapshot.finalize.double_check_complete
    {
        let changed_segment_ids = match double_check_after_resume(
            &job,
            &segments,
            &mut translations,
            &run_config,
            snapshot,
            &settings,
            progress.clone(),
            stop_cancel_token.clone(),
        )
        .await
        {
            Ok(changed_segment_ids) => changed_segment_ids,
            Err(_) if pause_signal.is_stopped() => {
                print_stopped_resume_hint(&job.id, print_stdout);
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        persist_corrected_translations(
            &store,
            &job.id,
            &run_config,
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
            print_stopped_resume_hint(&job.id, print_stdout);
            return Ok(());
        }
    }
    let block_translations = rebuild_block_translations(&segments, &stored_blocks, &translations);
    let rebuild_options = super::translate::rebuild_options_from_snapshot(snapshot);
    rebuild_epub_with_options(&book, &block_translations, &output, &rebuild_options)?;
    if validate_output {
        let validation_path = validate::default_report_path(&output);
        let validation = validate::validate_and_write(&output, &validation_path, false)?;
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
            print_stopped_resume_hint(&job.id, print_stdout);
            return Ok(());
        }
        if mark_job_finished(&store, &job.id, &translations)? {
            break;
        }
        if job_was_stopped(&store, &job.id)? {
            print_stopped_resume_hint(&job.id, print_stdout);
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
        println!(
            "Translated: {}/{} segments",
            summary.succeeded, summary.total_segments
        );
        println!("Cached: {}", summary.cached);
        println!("Retried: {}", summary.retried);
        println!("Needs review: {}", summary.needs_review);
        println!("Failed: {}", summary.failed);
        println!("Input tokens: {}", summary.input_tokens);
        println!("Output tokens: {}", summary.output_tokens);
        if let Some(cost) = crate::cost::estimate_cost_usd_with_cached(
            &job.provider,
            &job.model,
            summary.input_tokens,
            summary.input_cached_tokens,
            summary.output_tokens,
        ) {
            println!("Estimated cost: ${cost:.6}");
        }
        println!("Output: {}", output.display());
        println!("Report: {}", report.markdown.display());
        println!("Review: bookforge review {} --open", job.id);
    }

    Ok(())
}

fn human_stdout_enabled(ui: Option<crate::progress::UiMode>) -> bool {
    !matches!(
        ui,
        Some(
            crate::progress::UiMode::Json
                | crate::progress::UiMode::Quiet
                | crate::progress::UiMode::Tui
        )
    )
}

fn resolve_resume_input(job: &JobRecord, snapshot: &RunConfigSnapshot) -> Result<PathBuf> {
    if let Some(path) = snapshot
        .input_snapshot_path
        .as_ref()
        .or(job.input_snapshot_path.as_ref())
        && path.exists()
    {
        return Ok(path.clone());
    }

    if snapshot.input_snapshot_path.is_none() && job.input_snapshot_path.is_none() {
        tracing::warn!(
            "job '{}' predates input EPUB snapshots; falling back to original input path",
            job.id
        );
        if snapshot.input_path.exists() {
            return Ok(snapshot.input_path.clone());
        }
        anyhow::bail!(
            "job '{}' does not have an input snapshot and the original input path no longer exists: {}",
            job.id,
            snapshot.input_path.display()
        );
    }

    let snapshot_path = snapshot
        .input_snapshot_path
        .as_ref()
        .or(job.input_snapshot_path.as_ref())
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "<missing>".to_string());
    anyhow::bail!(
        "job '{}' input snapshot is missing: {}",
        job.id,
        snapshot_path
    )
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
        // Strictness is a scheduling choice, not part of the cached
        // translation contract, so it is not persisted in the snapshot.
        // Resumed jobs use the best-effort default.
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
            Ok(qa_reviews_for_mode(
                ProgressRequestProvider::new(provider, progress),
                segments,
                translations,
                &qa_run_config,
                qa_config,
                qa_mode,
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
            Ok(qa_reviews_for_mode(
                ProgressRequestProvider::new(provider, progress),
                segments,
                translations,
                &qa_run_config,
                qa_config,
                qa_mode,
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
        if !blocks.is_empty() {
            let (status, error) = status_by_segment
                .get(segment.id.0.as_str())
                .cloned()
                .unwrap_or(("succeeded", None));
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
            });
        }
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
    use bookforge_store::{CreateJob, SaveNeedsReview, SaveTranslation};
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
        let store_path = fixture.store.path().to_path_buf();
        let job_id = fixture.job.id.clone();
        let wake_id = job_id.clone();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            let store = JobStore::open(store_path).expect("store should reopen");
            store
                .mark_job_running(&wake_id)
                .expect("job should leave paused");
        });

        let resumed = wait_for_paused_job_to_leave_paused(
            &fixture.store,
            &job_id,
            Duration::from_secs(1),
            Duration::from_millis(1),
        )
        .await
        .expect("paused wait should not error");

        assert!(resumed, "live paused job should be observed leaving paused");
    }

    #[tokio::test]
    async fn paused_fast_resume_with_overrides_errors_with_apply_guidance() {
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

        let error = live_fast_resume_paused_job(&fixture.store, &args)
            .await
            .expect_err("pending overrides must block live fast-resume");
        let message = error.to_string();

        assert!(message.contains("live fast-resume cannot apply"));
        assert!(message.contains(&format!("bookforge stop {}", fixture.job.id)));
        assert!(message.contains(&format!("bookforge resume {}", fixture.job.id)));
        assert!(message.contains("--force"));
        assert_eq!(
            bookforge_core::read_control_file(&bookforge_core::control_path_for_job(
                &fixture.job.id
            ))
            .expect("control file should read"),
            ControlCommand::Run
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

    fn resume_fixture(settings: ResolvedRunSettings, segment_count: usize) -> ResumeFixture {
        let tempdir = tempfile::tempdir().expect("tempdir should be created");
        let input = fixture_input();
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
        let run_store = JobStore::open(fixture.store.path()).expect("store should reopen");
        let args = resume_args(&fixture.job.id);

        run_inner(
            args,
            run_store,
            fixture.job.clone(),
            &mut fixture.snapshot,
            overrides,
            sink,
        )
        .await?;

        Ok(events.lock().expect("events lock").clone())
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
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "bookforge-resume-fixture-{}-{nanos}.epub",
            std::process::id()
        ));
        build_resume_epub(&path);
        path
    }

    fn build_resume_epub(path: &Path) {
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
        zip.write_all(RESUME_CHAPTER_ONE.as_bytes()).unwrap();
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
