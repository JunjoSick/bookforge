use super::*;

pub async fn run(
    args: TranslateArgs,
    cancel_token: tokio_util::sync::CancellationToken,
) -> Result<()> {
    let settings = resolve_settings(&args);

    // Apply provider preset if specified (before explicit CLI overrides)
    let effective_provider = apply_provider_preset(&args.provider, args.provider_preset);

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

async fn run_mock_translation(
    input: &PathBuf,
    config: &TranslationConfig,
    provider_args: &CliProviderArgs,
    cli_args: &TranslateArgs,
    settings: &ResolvedRunSettings,
    progress: Arc<dyn bookforge_core::ProgressSink>,
) -> Result<()> {
    let started = std::time::Instant::now();
    progress.emit(bookforge_core::ProgressEvent::StageStarted {
        stage: "read_epub".to_string(),
        timestamp_ms: bookforge_core::progress::now_ms(),
    });
    let book = read_epub(input)?;
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
    let model = config
        .model
        .clone()
        .unwrap_or_else(|| "mock-prefix-target".to_string());
    let prompt_version = PromptVersion::V2.as_str();
    let store = JobStore::open_default()?;
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
        provider: "mock",
        model: &model,
        base_url: None,
        api_key_env: None,
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
        prompt_version,
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
        prompt_version,
        &cache_namespace,
        &glossary.fingerprint,
        &glossary.active_terms,
        &style.fingerprint,
        &style.rendered_block,
        &entities.fingerprint,
        &entities.rendered_block,
        &model,
        None,
        None,
    )?;
    let rebuild_options = rebuild_options_from_snapshot(&snapshot);
    store.insert_segments(
        &job.id,
        &segments,
        prompt_version,
        "mock",
        &model,
        &cache_namespace,
    )?;
    let pause_signal = bookforge_llm::PauseSignal::new();
    let stop_cancel_token = tokio_util::sync::CancellationToken::new();
    let control_watcher = crate::control::ControlFileWatcher::spawn_with_stop_cancel(
        store.path().to_path_buf(),
        job.id.clone(),
        progress.clone(),
        pause_signal.clone(),
        stop_cancel_token.clone(),
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
        provider: "mock".to_string(),
        model: model.clone(),
        prompt_version: prompt_version.to_string(),
        temperature: 0.2,
        scheduler: settings.scheduler.clone(),
        profile: settings.profile,
        model_context_tokens: None,
        max_output_tokens: None,
        batch_max_output_tokens: None,
        compact_prompts: false,
        glossary: glossary.run_config.clone(),
        context: context_run_config,
        context_registry: context_registry.clone(),
        style: style.run_config.clone(),
        entities: entities.run_config.clone(),
        pause_signal: Some(pause_signal.clone()),
        runtime_settings: Some(control_watcher.runtime_settings()),
    }; // mock
    let provider = MockProvider::new(mock_mode(&model), &config.target_language);
    let mut translations = apply_cached_translations(
        &segments,
        CacheContext {
            store: &store,
            job_id: &job.id,
            prompt_version,
            provider: &config.provider,
            model: &model,
            source_lang: config.source_language.as_deref(),
            target_lang: &config.target_language,
            cache_namespace: &cache_namespace,
        },
    )?;
    let pending_segments = pending_segments_for_job(&store, &job.id, &segments)?;
    prepopulate_context_registry(context_registry.as_ref(), &segments, &translations);
    progress.emit(bookforge_core::ProgressEvent::CacheScanFinished {
        hits: translations.len(),
        misses: pending_segments.len(),
        timestamp_ms: bookforge_core::progress::now_ms(),
    });
    let fresh_translations = run_checkpointed_translation(
        provider.clone(),
        &pending_segments,
        &run_config,
        settings,
        CheckpointRunContext {
            store: &store,
            job_id: &job.id,
            provider: "mock",
            model: &model,
            prompt_version,
        },
        progress.clone(),
        settings.batch.enabled,
    )
    .await?;
    if job_was_stopped(&store, &job.id)? {
        print_stopped_resume_hint(&job.id, human_stdout_enabled(cli_args.ui));
        return Ok(());
    }
    translations.extend(fresh_translations);
    translations.sort_by_key(|translation| translation.ordinal);
    let mut control_poller = crate::control::ControlFilePoller::new_with_stop_cancel(
        &store,
        &job.id,
        progress.clone(),
        stop_cancel_token.clone(),
    );
    if !wait_for_finalize_stage_control(&mut control_poller, &pause_signal).await? {
        print_stopped_resume_hint(&job.id, human_stdout_enabled(cli_args.ui));
        return Ok(());
    }
    let qa_runtime = job_runtime_settings.borrow().clone();
    let qa_run_config = crate::control::freeze_run_config_for_stage(&run_config, &qa_runtime);
    let qa_reviews = qa_reviews_for_mode(
        ProgressRequestProvider::new(provider.clone(), progress.clone()),
        &segments,
        &translations,
        &qa_run_config,
        &qa_runtime.settings.qa,
        qa_runtime.qa,
    )
    .await;
    if !wait_for_finalize_stage_control(&mut control_poller, &pause_signal).await? {
        print_stopped_resume_hint(&job.id, human_stdout_enabled(cli_args.ui));
        return Ok(());
    }
    let fallback_config = FallbackPassConfig::from_snapshot(snapshot.fallback.as_ref());
    let fallback_translations = run_fallback_pass(
        &stop_cancel_token,
        fallback_config.as_ref(),
        &segments,
        std::mem::take(&mut translations),
        &store,
        &job.id,
        prompt_version,
        settings,
        &run_config,
        Some(&mut control_poller),
        progress.clone(),
    )
    .await?;
    translations = fallback_translations;
    if job_was_stopped(&store, &job.id)? {
        print_stopped_resume_hint(&job.id, human_stdout_enabled(cli_args.ui));
        return Ok(());
    }
    if !wait_for_finalize_stage_control(&mut control_poller, &pause_signal).await? {
        print_stopped_resume_hint(&job.id, human_stdout_enabled(cli_args.ui));
        return Ok(());
    }
    let double_check_runtime = job_runtime_settings.borrow().clone();
    let double_check_run_config =
        crate::control::freeze_run_config_for_stage(&run_config, &double_check_runtime);
    if double_check_runtime.settings.double_check.mode != DoubleCheckMode::Off
        && !snapshot.finalize.double_check_complete
    {
        println!("Double-check: auditing translations...");
        let corrections = match run_double_check(
            ProgressRequestProvider::new(provider.clone(), progress.clone()),
            &segments,
            &translations,
            &double_check_run_config,
            &double_check_runtime.settings.double_check,
        )
        .await
        {
            Ok(corrections) => corrections,
            Err(_)
                if run_config
                    .pause_signal
                    .as_ref()
                    .is_some_and(bookforge_llm::PauseSignal::is_stopped) =>
            {
                print_stopped_resume_hint(&job.id, human_stdout_enabled(cli_args.ui));
                return Ok(());
            }
            Err(e) => return Err(anyhow::anyhow!("double-check failed: {e}")),
        };
        let changed_segment_ids = apply_double_check_corrections(&mut translations, &corrections);
        persist_corrected_translations(
            &store,
            &job.id,
            &double_check_run_config,
            &translations,
            &changed_segment_ids,
        )?;
        snapshot.finalize.double_check_complete = true;
        store.update_job_config_snapshot(&job.id, &snapshot)?;
        if job_was_stopped(&store, &job.id)? {
            print_stopped_resume_hint(&job.id, human_stdout_enabled(cli_args.ui));
            return Ok(());
        }
    }
    loop {
        if !wait_for_finalize_stage_control(&mut control_poller, &pause_signal).await? {
            print_stopped_resume_hint(&job.id, human_stdout_enabled(cli_args.ui));
            return Ok(());
        }
        if mark_job_finished(&store, &job.id, &translations)? {
            break;
        }
        if job_was_stopped(&store, &job.id)? {
            print_stopped_resume_hint(&job.id, human_stdout_enabled(cli_args.ui));
            return Ok(());
        }
    }
    let validation_runtime = job_runtime_settings.borrow().clone();
    print_summary_rebuild_and_report(
        &store,
        &job,
        &book,
        &segments,
        &translations,
        &qa_reviews,
        config,
        &rebuild_options,
        validation_runtime.validate_output,
        cli_args.strict_epubcheck,
        human_stdout_enabled(cli_args.ui),
    )?;
    let summary = store
        .summary(&job.id)?
        .ok_or_else(|| anyhow::anyhow!("job '{}' summary unavailable", job.id))?;
    reconfigure::clear_overrides_for_job(&job.id)?;
    progress.emit(bookforge_core::ProgressEvent::ArtifactWritten {
        path: config.output.display().to_string(),
        timestamp_ms: bookforge_core::progress::now_ms(),
    });
    progress.emit(bookforge_core::ProgressEvent::TranslationFinished {
        succeeded: summary.succeeded,
        cached: summary.cached,
        needs_review: summary.needs_review,
        failed: summary.failed,
        input_tokens: summary.input_tokens,
        output_tokens: summary.output_tokens,
        elapsed_ms: started.elapsed().as_millis() as u64,
        timestamp_ms: bookforge_core::progress::now_ms(),
    });

    Ok(())
}

async fn run_openai_compatible_translation(
    input: &PathBuf,
    config: &TranslationConfig,
    provider_args: &CliProviderArgs,
    cli_args: &TranslateArgs,
    settings: &ResolvedRunSettings,
    cancel_token: &tokio_util::sync::CancellationToken,
    progress: Arc<dyn bookforge_core::ProgressSink>,
) -> Result<()> {
    let started = std::time::Instant::now();
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

    progress.emit(bookforge_core::ProgressEvent::StageStarted {
        stage: "read_epub".to_string(),
        timestamp_ms: bookforge_core::progress::now_ms(),
    });
    let book = read_epub(input)?;
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
    let run_prompt_version = if settings.batch.enabled {
        PromptVersion::BatchV3.as_str()
    } else {
        PromptVersion::V2.as_str()
    };
    let store = JobStore::open_default()?;
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
        model: &model,
        base_url: Some(&provider_config.base_url),
        api_key_env: Some(&provider_config.api_key_env),
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
        run_prompt_version,
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
        run_prompt_version,
        &cache_namespace,
        &glossary.fingerprint,
        &glossary.active_terms,
        &style.fingerprint,
        &style.rendered_block,
        &entities.fingerprint,
        &entities.rendered_block,
        &model,
        Some(provider_config.base_url.clone()),
        Some(provider_config.api_key_env.clone()),
    )?;
    let rebuild_options = rebuild_options_from_snapshot(&snapshot);
    store.insert_segments(
        &job.id,
        &segments,
        run_prompt_version,
        &config.provider,
        &model,
        &cache_namespace,
    )?;
    let pause_signal = bookforge_llm::PauseSignal::new();
    let control_watcher = crate::control::ControlFileWatcher::spawn_with_stop_cancel(
        store.path().to_path_buf(),
        job.id.clone(),
        progress.clone(),
        pause_signal.clone(),
        cancel_token.clone(),
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
        model: model.clone(),
        prompt_version: run_prompt_version.to_string(),
        temperature: 0.2,
        scheduler: settings.scheduler.clone(),
        profile: settings.profile,
        model_context_tokens: settings.provider.model_context_tokens,
        max_output_tokens: settings.provider.max_output_tokens,
        batch_max_output_tokens: settings.provider.batch_max_output_tokens,
        compact_prompts: settings.compact_prompts,
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
            prompt_version: run_prompt_version,
            provider: &config.provider,
            model: &model,
            source_lang: config.source_language.as_deref(),
            target_lang: &config.target_language,
            cache_namespace: &cache_namespace,
        },
    )?;
    let hits = translations.len();
    let pending_count = segments.len().saturating_sub(hits);
    progress.emit(bookforge_core::ProgressEvent::CacheScanFinished {
        hits,
        misses: pending_count,
        timestamp_ms: bookforge_core::progress::now_ms(),
    });
    let pending_segments = pending_segments_for_job(&store, &job.id, &segments)?;
    prepopulate_context_registry(context_registry.as_ref(), &segments, &translations);
    let fresh_translations = run_checkpointed_translation(
        provider.clone(),
        &pending_segments,
        &run_config,
        settings,
        CheckpointRunContext {
            store: &store,
            job_id: &job.id,
            provider: &config.provider,
            model: &model,
            prompt_version: run_prompt_version,
        },
        progress.clone(),
        settings.batch.enabled,
    )
    .await?;
    if job_was_stopped(&store, &job.id)? {
        print_stopped_resume_hint(&job.id, human_stdout_enabled(cli_args.ui));
        return Ok(());
    }
    translations.extend(fresh_translations);

    finish_translation_pipeline(
        &provider,
        cancel_token,
        cli_args,
        &segments,
        &mut translations,
        &store,
        &job,
        run_prompt_version,
        settings,
        &run_config,
        config,
        &rebuild_options,
        &book,
        progress.clone(),
        started,
        &mut snapshot,
        &job_runtime_settings,
    )
    .await?;

    if cancel_token.is_cancelled() && !job_was_stopped(&store, &job.id)? {
        let _ = store.mark_job_interrupted(&job.id);
        eprintln!();
        eprintln!("Interrupted by user.");
        eprintln!("Your progress has been saved to job: {}", job.id);
        eprintln!();
        eprintln!("Resume with:");
        eprintln!("  bookforge resume {}", job.id);
        return Ok(());
    }

    Ok(())
}
