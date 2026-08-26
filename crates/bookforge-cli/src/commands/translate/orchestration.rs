use super::*;

pub async fn run(
    args: TranslateArgs,
    cancel_token: tokio_util::sync::CancellationToken,
) -> Result<()> {
    // Apply provider preset if specified (before explicit CLI overrides)
    let effective_provider = apply_provider_preset(&args.provider, args.provider_preset);
    let (settings, plan_application) = resolve_settings_and_plan(&args, &effective_provider)?;

    let output = args
        .out
        .clone()
        .unwrap_or_else(|| default_output_path(&args.input, &args.language.target));
    let config = TranslationConfig {
        source_language: args.language.source.clone(),
        target_language: args.language.target.clone(),
        provider: effective_provider.provider.clone(),
        model: effective_provider.model.clone(),
        concurrency: settings.scheduler.concurrency,
        max_attempts: settings.scheduler.max_attempts,
        output,
    };

    if human_stdout_enabled(args.ui) {
        println!("Input: {}", args.input.display());
        println!("Output: {}", config.output.display());
        println!("Target: {}", config.target_language);
        println!("Provider: {}", config.provider);
        println!("Profile: {:?}", args.profile);
        println!("Concurrency: {}", config.concurrency);
        println!("Batch enabled: {}", settings.batch.enabled);

        if settings.batch.enabled {
            println!("Batch target tokens: {}", settings.batch.target_tokens);
            println!("Batch max items: {}", settings.batch.max_items);
        }
    }

    let reporter = crate::progress::ProgressReporter::spawn_with_options(
        args.ui,
        args.progress_jsonl.clone(),
        false,
        Some(cancel_token.clone()),
    );
    let progress_sink = reporter.sink();

    if let Some(message) = retry_amplification_warning(&settings) {
        progress_sink.emit(bookforge_core::ProgressEvent::Warning {
            kind: "retry_amplification".to_string(),
            message: message.clone(),
            timestamp_ms: bookforge_core::progress::now_ms(),
        });
        eprintln!("warn: {message}");
    }

    let run_result = async {
        match config.provider.as_str() {
            "mock" => {
                run_mock_translation(
                    &args.input,
                    &config,
                    &effective_provider,
                    &args,
                    &settings,
                    plan_application,
                    &cancel_token,
                    progress_sink,
                )
                .await
            }
            "deepseek" | "openrouter" | "openai-compatible" => {
                run_openai_compatible_translation(
                    &args.input,
                    &config,
                    &effective_provider,
                    &args,
                    &settings,
                    plan_application,
                    &cancel_token,
                    progress_sink,
                )
                .await
            }
            _ => {
                anyhow::bail!("unsupported translation provider '{}'", config.provider)
            }
        }
    }
    .await;

    finalize_reporter(run_result, reporter).await
}

pub(super) struct PlanApplication {
    pub(super) book: bookforge_core::ir::Book,
    pub(super) applied_plan: bookforge_core::run_snapshot::AppliedPlanSnapshot,
}

pub(super) fn resolve_settings_and_plan(
    args: &TranslateArgs,
    effective_provider: &CliProviderArgs,
) -> Result<(ResolvedRunSettings, Option<PlanApplication>)> {
    let mut settings = resolve_settings(args);
    if !args.plan {
        return Ok((settings, None));
    }

    let book = read_epub(&args.input)?;
    let plan = crate::commands::plan::plan_book(
        &book,
        &args.input,
        args.language.source.as_deref(),
        &args.language.target,
        &effective_provider.provider,
        effective_provider.model.as_deref(),
    )?;
    let applied_plan = apply_plan_recommendations(args, &mut settings, &plan);
    Ok((settings, Some(PlanApplication { book, applied_plan })))
}

pub(super) fn human_stdout_enabled(ui: crate::progress::UiMode) -> bool {
    // The TUI owns the screen, so suppress plain stdout/stderr prints that would
    // corrupt it; the dashboard surfaces the same information.
    !matches!(
        ui,
        crate::progress::UiMode::Json
            | crate::progress::UiMode::Quiet
            | crate::progress::UiMode::Tui
    )
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

#[allow(clippy::too_many_arguments)]
async fn run_mock_translation(
    input: &PathBuf,
    config: &TranslationConfig,
    provider_args: &CliProviderArgs,
    cli_args: &TranslateArgs,
    settings: &ResolvedRunSettings,
    plan_application: Option<PlanApplication>,
    cancel_token: &tokio_util::sync::CancellationToken,
    progress: Arc<dyn bookforge_core::ProgressSink>,
) -> Result<()> {
    run_mock_translation_with_store(
        input,
        config,
        provider_args,
        cli_args,
        settings,
        plan_application,
        cancel_token,
        progress,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_mock_translation_with_store(
    input: &PathBuf,
    config: &TranslationConfig,
    provider_args: &CliProviderArgs,
    cli_args: &TranslateArgs,
    settings: &ResolvedRunSettings,
    plan_application: Option<PlanApplication>,
    cancel_token: &tokio_util::sync::CancellationToken,
    progress: Arc<dyn bookforge_core::ProgressSink>,
    store: Option<JobStore>,
) -> Result<()> {
    let model = config
        .model
        .clone()
        .unwrap_or_else(|| "mock-prefix-target".to_string());
    let provider = MockProvider::new(mock_mode(&model), &config.target_language);
    run_translation_with_store(
        input,
        config,
        provider_args,
        cli_args,
        settings,
        plan_application,
        ProviderRun {
            provider,
            model,
            prompt_version: if settings.batch.enabled {
                PromptVersion::BatchV3.as_str()
            } else {
                PromptVersion::V2.as_str()
            },
            base_url: None,
            api_key_env: None,
            model_context_tokens: settings.provider.model_context_tokens,
            max_output_tokens: settings.provider.max_output_tokens,
            batch_max_output_tokens: settings.provider.batch_max_output_tokens,
            compact_prompts: settings.compact_prompts,
            cancel_token: cancel_token.clone(),
        },
        progress,
        store,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_openai_compatible_translation(
    input: &PathBuf,
    config: &TranslationConfig,
    provider_args: &CliProviderArgs,
    cli_args: &TranslateArgs,
    settings: &ResolvedRunSettings,
    plan_application: Option<PlanApplication>,
    cancel_token: &tokio_util::sync::CancellationToken,
    progress: Arc<dyn bookforge_core::ProgressSink>,
) -> Result<()> {
    run_openai_compatible_translation_with_store(
        input,
        config,
        provider_args,
        cli_args,
        settings,
        plan_application,
        cancel_token,
        progress,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_openai_compatible_translation_with_store(
    input: &PathBuf,
    config: &TranslationConfig,
    provider_args: &CliProviderArgs,
    cli_args: &TranslateArgs,
    settings: &ResolvedRunSettings,
    plan_application: Option<PlanApplication>,
    cancel_token: &tokio_util::sync::CancellationToken,
    progress: Arc<dyn bookforge_core::ProgressSink>,
    store: Option<JobStore>,
) -> Result<()> {
    let mut provider_config = provider_config(
        &config.provider,
        config.model.as_deref(),
        provider_args.base_url.as_deref(),
        provider_args.api_key_env.as_deref(),
        settings.provider.timeout_seconds,
        settings.provider.provider_max_attempts,
        settings.provider.thinking_disabled,
        settings.provider.retry_after_policy,
        settings.provider.max_backoff_seconds,
        settings.provider.max_idle_per_host,
        settings.provider.json_mode,
    )?;
    provider_config.json_mode = settings.provider.json_mode;
    let provider =
        OpenAiCompatibleProvider::new_with_cancel(provider_config.clone(), cancel_token.clone())?;
    let model = provider.model().to_string();
    progress.emit(bookforge_core::ProgressEvent::RuntimeConfigResolved {
        profile: format!("{:?}", settings.profile),
        provider_preset: cli_args.provider_preset.map(|preset| format!("{preset:?}")),
        provider: config.provider.clone(),
        model: model.clone(),
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
        timestamp_ms: bookforge_core::progress::now_ms(),
    });

    run_translation_with_store(
        input,
        config,
        provider_args,
        cli_args,
        settings,
        plan_application,
        ProviderRun {
            provider,
            model,
            prompt_version: if settings.batch.enabled {
                PromptVersion::BatchV3.as_str()
            } else {
                PromptVersion::V2.as_str()
            },
            base_url: Some(provider_config.base_url),
            api_key_env: Some(provider_config.api_key_env),
            model_context_tokens: settings.provider.model_context_tokens,
            max_output_tokens: settings.provider.max_output_tokens,
            batch_max_output_tokens: settings.provider.batch_max_output_tokens,
            compact_prompts: settings.compact_prompts,
            cancel_token: cancel_token.clone(),
        },
        progress,
        store,
    )
    .await
}

struct ProviderRun<P> {
    provider: P,
    model: String,
    prompt_version: &'static str,
    base_url: Option<String>,
    api_key_env: Option<String>,
    model_context_tokens: Option<u32>,
    max_output_tokens: Option<u32>,
    batch_max_output_tokens: Option<u32>,
    compact_prompts: bool,
    cancel_token: tokio_util::sync::CancellationToken,
}

#[allow(clippy::too_many_arguments)]
async fn run_translation_with_store<P>(
    input: &PathBuf,
    config: &TranslationConfig,
    provider_args: &CliProviderArgs,
    cli_args: &TranslateArgs,
    settings: &ResolvedRunSettings,
    plan_application: Option<PlanApplication>,
    provider_run: ProviderRun<P>,
    progress: Arc<dyn bookforge_core::ProgressSink>,
    store: Option<JobStore>,
) -> Result<()>
where
    P: LlmProvider + Clone,
{
    let started = std::time::Instant::now();
    progress.emit(bookforge_core::ProgressEvent::StageStarted {
        stage: "read_epub".to_string(),
        timestamp_ms: bookforge_core::progress::now_ms(),
    });
    let (book, applied_plan) = match plan_application {
        Some(application) => (application.book, Some(application.applied_plan)),
        None => (read_epub(input)?, None),
    };
    progress.emit(bookforge_core::ProgressEvent::StageFinished {
        stage: "read_epub".to_string(),
        timestamp_ms: bookforge_core::progress::now_ms(),
    });

    progress.emit(bookforge_core::ProgressEvent::StageStarted {
        stage: "segmentation".to_string(),
        timestamp_ms: bookforge_core::progress::now_ms(),
    });
    let segments = build_segments(&book, &settings.segmentation)?;
    progress.emit(bookforge_core::ProgressEvent::SegmentationFinished {
        segment_count: segments.len(),
        timestamp_ms: bookforge_core::progress::now_ms(),
    });
    let store = match store {
        Some(store) => store,
        None => JobStore::open_default()?,
    };
    let glossary = prepare_glossary_run_config(
        &store,
        &cli_args.glossary,
        config.source_language.as_deref(),
        &config.target_language,
        cli_args.book_id.as_deref(),
        cli_args.series_id.as_deref(),
        cli_args.glossary_format,
        cli_args.glossary_budget_tokens,
        cli_args.prompt_extra.clone(),
        &segments,
    )?;
    let glossary_rules = select_glossary_for_segments(
        &segments,
        &glossary.active_terms,
        cli_args.glossary_budget_tokens,
    )
    .rules_by_segment;
    let context_run_config = context_run_config_from_args(cli_args);
    let context_registry: Option<Arc<ContextRegistry>> = if context_run_config.enabled() {
        Some(Arc::new(ContextRegistry::new(&segments)))
    } else {
        None
    };
    let style = prepare_style_run_config(
        &store,
        &cli_args.style,
        &config.target_language,
        cli_args.book_id.as_deref(),
        cli_args.series_id.as_deref(),
    )?;
    let entities = prepare_entities_run_config(
        &store,
        &cli_args.entities,
        config.source_language.as_deref(),
        &config.target_language,
        cli_args.book_id.as_deref(),
        cli_args.series_id.as_deref(),
    )?;
    let job = store.create_job(CreateJob {
        input,
        output: &config.output,
        source_lang: config.source_language.as_deref(),
        target_lang: &config.target_language,
        provider: &config.provider,
        model: &provider_run.model,
        base_url: provider_run.base_url.as_deref(),
        api_key_env: provider_run.api_key_env.as_deref(),
        book_id: cli_args.book_id.as_deref(),
        series_id: cli_args.series_id.as_deref(),
    })?;
    if human_stdout_enabled(cli_args.ui) {
        println!("Job: {}", job.id);
    }
    crate::control::clear_job_control(&job.id)?;
    progress.emit(bookforge_core::ProgressEvent::JobCreated {
        job_id: job.id.clone(),
        input_path: input.display().to_string(),
        output_path: config.output.display().to_string(),
        timestamp_ms: bookforge_core::progress::now_ms(),
    });
    let cache_namespace = compute_cache_namespace(
        settings.segmentation.max_segment_tokens,
        settings.segmentation.context_tokens,
        settings.profile.namespace_str(),
        settings.batch.enabled,
        provider_run.prompt_version,
        &glossary.fingerprint,
        if style.run_config.is_some() {
            &style.fingerprint
        } else {
            ""
        },
        if entities.run_config.is_some() {
            &entities.fingerprint
        } else {
            ""
        },
    );
    let mut snapshot = persist_snapshot(
        &store,
        &job,
        input,
        &config.output,
        provider_args,
        cli_args,
        settings,
        provider_run.prompt_version,
        &cache_namespace,
        &glossary.fingerprint,
        &glossary.active_terms,
        &style.fingerprint,
        &style.rendered_block,
        &entities.fingerprint,
        &entities.rendered_block,
        &provider_run.model,
        provider_run.base_url.clone(),
        provider_run.api_key_env.clone(),
        applied_plan.as_ref(),
    )?;
    let rebuild_options = rebuild_options_from_snapshot(&snapshot);
    store.insert_segments(
        &job.id,
        &segments,
        provider_run.prompt_version,
        &config.provider,
        &provider_run.model,
        &cache_namespace,
    )?;
    let pause_signal = bookforge_llm::PauseSignal::new();
    let control_watcher = crate::control::ControlFileWatcher::spawn_with_stop_cancel(
        store.path().to_path_buf(),
        job.id.clone(),
        progress.clone(),
        pause_signal.clone(),
        provider_run.cancel_token.clone(),
        crate::control::ControlBaseline {
            settings: settings.clone(),
            qa: cli_args.qa,
            validate_output: cli_args.validate_output,
        },
    );
    let job_runtime_settings = control_watcher.job_runtime_settings();
    let run_config = TranslationRunConfig {
        source_language: config.source_language.clone(),
        target_language: config.target_language.clone(),
        provider: config.provider.clone(),
        model: provider_run.model.clone(),
        prompt_version: provider_run.prompt_version.to_string(),
        temperature: 0.2,
        scheduler: settings.scheduler.clone(),
        profile: settings.profile,
        model_context_tokens: provider_run.model_context_tokens,
        max_output_tokens: provider_run.max_output_tokens,
        batch_max_output_tokens: provider_run.batch_max_output_tokens,
        compact_prompts: provider_run.compact_prompts,
        glossary: glossary.run_config.clone(),
        context: context_run_config,
        context_registry: context_registry.clone(),
        style: style.run_config.clone(),
        entities: entities.run_config.clone(),
        pause_signal: Some(pause_signal.clone()),
        runtime_settings: Some(control_watcher.runtime_settings()),
    };
    let mut translations = apply_cached_translations(
        &segments,
        CacheContext {
            store: &store,
            job_id: &job.id,
            prompt_version: provider_run.prompt_version,
            provider: &config.provider,
            model: &provider_run.model,
            source_lang: config.source_language.as_deref(),
            target_lang: &config.target_language,
            cache_namespace: &cache_namespace,
        },
    )?;
    let pending_segments = pending_segments_for_job(&store, &job.id, &segments)?;
    progress.emit(bookforge_core::ProgressEvent::CacheScanFinished {
        hits: translations.len(),
        misses: pending_segments.len(),
        timestamp_ms: bookforge_core::progress::now_ms(),
    });
    prepopulate_context_registry(context_registry.as_ref(), &segments, &translations);
    let telemetry = Arc::new(TelemetryLog::new());
    let fresh_translations = run_checkpointed_translation_instrumented(
        provider_run.provider.clone(),
        &pending_segments,
        &run_config,
        settings,
        CheckpointRunContext {
            store: &store,
            job_id: &job.id,
            provider: &config.provider,
            model: &provider_run.model,
            prompt_version: provider_run.prompt_version,
        },
        progress.clone(),
        settings.batch.enabled,
        telemetry.clone(),
        &glossary_rules,
        human_stdout_enabled(cli_args.ui),
    )
    .await?;
    if job_was_stopped(&store, &job.id)? {
        print_stopped_resume_hint(&job.id, human_stdout_enabled(cli_args.ui));
        return Ok(());
    }
    translations.extend(fresh_translations);

    finish_translation_pipeline(
        &provider_run.provider,
        &provider_run.cancel_token,
        cli_args,
        &segments,
        &mut translations,
        &store,
        &job,
        provider_run.prompt_version,
        settings,
        &run_config,
        config,
        &rebuild_options,
        &book,
        progress.clone(),
        started,
        &mut snapshot,
        &job_runtime_settings,
        telemetry.as_ref(),
        &glossary_rules,
    )
    .await
    .inspect_err(|error| {
        // Hard finalize failures (rebuild, validation, fallback
        // misconfiguration) must not leave the job stuck in "running"
        // forever; only doctor/dashboard would otherwise hint at the truth.
        mark_run_failed_on_error(&store, &job.id, error);
    })?;

    if telemetry.has_glossary_entries() {
        let summary = telemetry.glossary_summary();
        tracing::info!(target: "bookforge::telemetry", "{summary}");
        if human_stdout_enabled(cli_args.ui) {
            eprintln!("{summary}");
        }
    }

    if provider_run.cancel_token.is_cancelled() && !job_was_stopped(&store, &job.id)? {
        let _ = store.mark_job_interrupted(&job.id);
        if human_stdout_enabled(cli_args.ui) {
            eprintln!();
            eprintln!("Interrupted by user.");
            eprintln!("Your progress has been saved to job: {}", job.id);
            eprintln!();
            eprintln!("Resume with:");
            eprintln!("  bookforge resume {}", job.id);
        }
        // UI-21: a user interruption is reported as 130 (128+SIGINT), not a
        // silent success — progress is saved, but the run did not finish.
        crate::exit_code::request(crate::exit_code::INTERRUPTED);
        return Ok(());
    }

    Ok(())
}
