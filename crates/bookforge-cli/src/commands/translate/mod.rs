use anyhow::Result;
#[cfg(test)]
use bookforge_core::config::TranslationProfile;
use bookforge_core::{
    FallbackRunConfigSnapshot, GlossaryFormat, GlossaryTerm, NullProgressSink, RunConfigSnapshot,
    config::{
        DoubleCheckMode, FallbackScope, PromptVersion, ResolvedRunSettings, TranslationConfig,
    },
    marker::{parse_empty_marker, parse_marker_close, parse_paired_marker_open},
    merge_scope_terms,
    scheduler::SchedulerConfig,
    segment::{Segment, SegmentStatus, build_segments, compute_cache_namespace},
    select_glossary_for_segments,
};
use bookforge_epub::read_epub;
#[cfg(test)]
use bookforge_llm::translate_segments;
use bookforge_llm::{
    AdaptiveLimiter, CompletionRequest, CompletionResponse, ContextRegistry, ContextRunConfig,
    EntityRunConfig, GlossaryRunConfig, LlmError, LlmProvider, MockMode, MockProvider,
    OpenAiCompatibleConfig, OpenAiCompatibleProvider, ProviderCapabilities, ProviderRateController,
    QaSegmentReview, RateControllerConfig, SegmentTranslation, StyleRunConfig, TelemetryLog,
    TranslationRunConfig, account_for_batch_prompt_overhead, build_translation_batches,
    qa_segments_parallel, run_double_check, telemetry_summary, translate_batches_with_callback,
    translate_batches_with_control, translate_segments_with_callback,
    translate_segments_with_control,
};
use bookforge_store::{CreateJob, JobRecord, JobStore, SaveTranslation};
use clap::Args;
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

#[cfg(test)]
use crate::LanguageArgs;
use crate::{
    ProviderArgs as CliProviderArgs, QaMode,
    checkpoint::{CheckpointCommand, CheckpointSender, CheckpointWriter},
    commands::{glossary::read_glossary_file, reconfigure},
    default_output_path,
};

pub mod args;
mod cache;
mod checkpointing;
mod engine;
mod reporting;
mod settings;
mod snapshot;

pub use args::TranslateArgs;
pub(crate) use cache::{CacheContext, apply_cached_translations, pending_segments_for_job};
use checkpointing::finalize_writer;
pub(crate) use engine::{CheckpointRunContext, run_checkpointed_translation};
use reporting::print_summary_rebuild_and_report;
pub(crate) use reporting::{rebuild_options_from_snapshot, regenerate_report_after_correction};
use settings::{apply_provider_preset, resolve_settings, retry_amplification_warning};
use snapshot::persist_snapshot;

#[derive(Debug, Clone)]
pub(crate) struct FallbackPassConfig {
    provider: String,
    model: String,
    base_url: Option<String>,
    api_key_env: Option<String>,
    scope: FallbackScope,
}

impl FallbackPassConfig {
    pub(crate) fn from_snapshot(snapshot: Option<&FallbackRunConfigSnapshot>) -> Option<Self> {
        snapshot.map(|fallback| Self {
            provider: fallback.provider.clone(),
            model: fallback.model.clone(),
            base_url: fallback.base_url.clone(),
            api_key_env: fallback.api_key_env.clone(),
            scope: fallback.scope,
        })
    }
}

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

fn human_stdout_enabled(ui: crate::progress::UiMode) -> bool {
    // The TUI owns the screen, so suppress plain stdout/stderr prints that would
    // corrupt it; the dashboard surfaces the same information.
    !matches!(
        ui,
        crate::progress::UiMode::Json
            | crate::progress::UiMode::Quiet
            | crate::progress::UiMode::Tui
    )
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedGlossary {
    pub run_config: GlossaryRunConfig,
    pub fingerprint: String,
    pub active_terms: Vec<GlossaryTerm>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn context_run_config_from_args(cli_args: &TranslateArgs) -> ContextRunConfig {
    ContextRunConfig {
        window: cli_args.context_window,
        budget_tokens: cli_args.context_budget_tokens,
        scope: cli_args.context_scope,
        strict: cli_args.context_strict,
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedStyle {
    pub run_config: Option<StyleRunConfig>,
    pub fingerprint: String,
    pub rendered_block: String,
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedEntities {
    pub run_config: Option<EntityRunConfig>,
    pub fingerprint: String,
    pub rendered_block: String,
}

/// Import `--entities` files into the store, then load all entities
/// applicable to the current (source_language, target_language, book_id,
/// series_id) scope and produce both the runtime injection bundle and
/// the snapshot-capture fields.
pub(crate) fn prepare_entities_run_config(
    store: &JobStore,
    entity_files: &[PathBuf],
    source_language: Option<&str>,
    target_language: &str,
    book_id: Option<&str>,
    series_id: Option<&str>,
) -> Result<PreparedEntities> {
    for path in entity_files {
        let entities = crate::commands::entity::read_entities_file(path)?;
        crate::commands::entity::upsert_entities(store, &entities)?;
    }
    // Without a source language we can't load entities (they're keyed on
    // both languages). The run still needs a stable fingerprint.
    let Some(source_language) = source_language else {
        let fp = bookforge_core::entity::entities_fingerprint(&[]);
        return Ok(PreparedEntities {
            run_config: None,
            fingerprint: fp,
            rendered_block: String::new(),
        });
    };
    let stored =
        store.load_active_entities(source_language, target_language, book_id, series_id)?;
    let active: Vec<bookforge_core::entity::Entity> = stored
        .into_iter()
        .map(|r| bookforge_core::entity::Entity {
            id: Some(r.id),
            scope_kind: r.scope_kind,
            scope_id: r.scope_id,
            source_name: r.source_name,
            target_name: r.target_name,
            gender_target: r.gender_target,
            role: r.role,
            notes: r.notes,
            source_language: r.source_language,
            target_language: r.target_language,
        })
        .collect();
    let merged = bookforge_core::entity::merge_scope_entities(&active);
    let rendered_block = bookforge_core::entity::render_entity_agreement_block(&merged);
    let fingerprint = bookforge_core::entity::entities_fingerprint(&merged);
    let run_config = if rendered_block.is_empty() {
        None
    } else {
        Some(EntityRunConfig {
            rendered_block: rendered_block.clone(),
            fingerprint: fingerprint.clone(),
        })
    };
    Ok(PreparedEntities {
        run_config,
        fingerprint,
        rendered_block,
    })
}

/// Load `--style` files into the store, merge all sheets for the active
/// scope, render the prompt block, and produce both the run-time
/// injection bundle and the snapshot-capture fields.
pub(crate) fn prepare_style_run_config(
    store: &JobStore,
    style_files: &[PathBuf],
    target_language: &str,
    book_id: Option<&str>,
    series_id: Option<&str>,
) -> Result<PreparedStyle> {
    for path in style_files {
        let sheet = crate::commands::style::read_style_file(path)?;
        let content_toml = std::fs::read_to_string(path)?;
        let one_sheet = vec![sheet.clone()];
        let merged_for_one = bookforge_core::style::merge_style_sheets(&one_sheet);
        let fp = bookforge_core::style::style_fingerprint(merged_for_one.as_ref());
        store.upsert_style_sheet(&bookforge_store::NewStyleSheet {
            scope_kind: sheet.scope_kind,
            scope_id: sheet.scope_id.as_deref(),
            target_language: &sheet.target_language,
            content_toml: &content_toml,
            fingerprint: &fp,
        })?;
    }
    let stored = store.load_active_style_sheets(target_language, book_id, series_id)?;
    let mut parsed: Vec<bookforge_core::style::StyleSheet> = Vec::new();
    for record in &stored {
        match crate::commands::style::parse_style_toml(&record.content_toml) {
            Ok(sheet) => parsed.push(sheet),
            Err(err) => tracing::warn!(
                style_id = record.id,
                "skipping stored style sheet that failed to parse: {err}"
            ),
        }
    }
    let merged = bookforge_core::style::merge_style_sheets(&parsed);
    let rendered_block = bookforge_core::style::render_style_block(merged.as_ref());
    let fingerprint = bookforge_core::style::style_fingerprint(merged.as_ref());
    let run_config = if rendered_block.is_empty() {
        None
    } else {
        Some(StyleRunConfig {
            rendered_block: rendered_block.clone(),
            fingerprint: fingerprint.clone(),
        })
    };
    Ok(PreparedStyle {
        run_config,
        fingerprint,
        rendered_block,
    })
}

/// Seed the in-memory completion fence from already-completed translations
/// (cache hits, prior runs). Without this the fence would deadlock on
/// segments that won't be re-translated this run.
pub(crate) fn prepopulate_context_registry(
    registry: Option<&Arc<ContextRegistry>>,
    segments: &[Segment],
    translations: &[SegmentTranslation],
) {
    let Some(registry) = registry else {
        return;
    };
    let by_id: std::collections::HashMap<_, _> = segments.iter().map(|s| (&s.id, s)).collect();
    for translation in translations {
        if let Some(segment) = by_id.get(&translation.segment_id) {
            registry.pre_populate(segment, translation);
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_glossary_run_config(
    store: &JobStore,
    glossary_files: &[PathBuf],
    source_language: Option<&str>,
    target_language: &str,
    book_id: Option<&str>,
    series_id: Option<&str>,
    format: GlossaryFormat,
    budget_tokens: usize,
    prompt_extra: Option<String>,
    segments: &[Segment],
) -> Result<PreparedGlossary> {
    let imported_terms = import_glossary_files(store, glossary_files)?;
    let mut active_terms = if let Some(source_language) = source_language {
        store.load_active_glossary_terms(source_language, target_language, book_id, series_id)?
    } else {
        store.load_active_glossary_terms_for_target(target_language, book_id, series_id)?
    };
    active_terms.extend(imported_terms.into_iter().filter(|term| {
        term.active()
            && term.target_language == target_language
            && source_language.is_none_or(|source| term.source_language == source)
    }));
    active_terms = merge_scope_terms(&active_terms);
    let selected = select_glossary_for_segments(segments, &active_terms, budget_tokens);
    if selected.truncated_authoritative_entries > 0 {
        tracing::warn!(
            count = selected.truncated_authoritative_entries,
            "glossary token budget dropped authoritative entries"
        );
    }
    let fingerprint = glossary_fingerprint(
        format,
        budget_tokens,
        prompt_extra.as_deref(),
        &active_terms,
    );
    Ok(PreparedGlossary {
        run_config: GlossaryRunConfig {
            format,
            entries_by_segment: selected.entries_by_segment,
            prompt_extra,
            guidance_by_segment: std::collections::HashMap::new(),
        },
        fingerprint,
        active_terms,
    })
}

fn import_glossary_files(
    store: &JobStore,
    glossary_files: &[PathBuf],
) -> Result<Vec<GlossaryTerm>> {
    let mut imported = Vec::new();
    for path in glossary_files {
        let terms = read_glossary_file(path)?;
        store.upsert_glossary_terms(&terms)?;
        imported.extend(terms);
    }
    Ok(imported)
}

pub(crate) fn glossary_fingerprint(
    format: GlossaryFormat,
    budget_tokens: usize,
    prompt_extra: Option<&str>,
    terms: &[GlossaryTerm],
) -> String {
    let mut normalized = terms.to_vec();
    for term in &mut normalized {
        term.id = None;
    }
    normalized.sort_by(|a, b| {
        a.source_language
            .cmp(&b.source_language)
            .then_with(|| a.target_language.cmp(&b.target_language))
            .then_with(|| a.scope_kind.priority().cmp(&b.scope_kind.priority()))
            .then_with(|| a.scope_id.cmp(&b.scope_id))
            .then_with(|| a.source_text.cmp(&b.source_text))
            .then_with(|| a.target_text.cmp(&b.target_text))
    });
    let payload = serde_json::json!({
        "schema": 1,
        "format": format.as_str(),
        "budget_tokens": budget_tokens,
        "prompt_extra": prompt_extra.unwrap_or(""),
        "terms": normalized,
    });
    let serialized = serde_json::to_vec(&payload).unwrap_or_default();
    let digest = Sha256::digest(serialized);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut hex, "{byte:02x}").expect("writing to string cannot fail");
    }
    hex
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
        settings.clone(),
        cli_args.qa,
        cli_args.validate_output,
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

pub(crate) fn mock_mode(model: &str) -> MockMode {
    match model {
        "mock-identity" => MockMode::Identity,
        "mock-uppercase" => MockMode::Uppercase,
        "mock-malformed-json" => MockMode::MalformedJson,
        "mock-wrong-segment-id" => MockMode::WrongSegmentId,
        _ => MockMode::PrefixTarget,
    }
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
        settings.clone(),
        cli_args.qa,
        cli_args.validate_output,
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

/// Shared post-translation pipeline: QA, fallback, double-check, finish, report.
/// Both batch and non-batch paths call this after translation completes.
#[allow(clippy::too_many_arguments)]
async fn finish_translation_pipeline(
    provider: &OpenAiCompatibleProvider,
    cancel_token: &tokio_util::sync::CancellationToken,
    cli_args: &TranslateArgs,
    segments: &[Segment],
    translations: &mut Vec<SegmentTranslation>,
    store: &JobStore,
    job: &JobRecord,
    run_prompt_version: &str,
    settings: &ResolvedRunSettings,
    run_config: &TranslationRunConfig,
    config: &TranslationConfig,
    rebuild_options: &bookforge_epub::RebuildOptions,
    book: &bookforge_core::ir::Book,
    progress: Arc<dyn bookforge_core::ProgressSink>,
    started: std::time::Instant,
    snapshot: &mut RunConfigSnapshot,
    job_runtime_settings: &tokio::sync::watch::Receiver<crate::control::JobRuntimeSettings>,
) -> Result<()> {
    translations.sort_by_key(|t| t.ordinal);

    let pause_signal = run_config.pause_signal.clone().unwrap_or_default();
    let mut controlled_run_config = run_config.clone();
    controlled_run_config.pause_signal = Some(pause_signal.clone());
    let mut control_poller = crate::control::ControlFilePoller::new_with_stop_cancel(
        store,
        &job.id,
        progress.clone(),
        cancel_token.clone(),
    );

    if !wait_for_finalize_stage_control(&mut control_poller, &pause_signal).await? {
        print_stopped_resume_hint(&job.id, human_stdout_enabled(cli_args.ui));
        return Ok(());
    }
    let qa_runtime = job_runtime_settings.borrow().clone();
    let qa_run_config =
        crate::control::freeze_run_config_for_stage(&controlled_run_config, &qa_runtime);
    let qa_reviews = qa_reviews_for_mode(
        ProgressRequestProvider::new(provider.clone(), progress.clone()),
        segments,
        translations,
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
        cancel_token,
        fallback_config.as_ref(),
        segments,
        std::mem::take(translations),
        store,
        &job.id,
        run_prompt_version,
        settings,
        &controlled_run_config,
        Some(&mut control_poller),
        progress.clone(),
    )
    .await?;
    *translations = fallback_translations;
    if job_was_stopped(store, &job.id)? {
        print_stopped_resume_hint(&job.id, human_stdout_enabled(cli_args.ui));
        return Ok(());
    }

    if !wait_for_finalize_stage_control(&mut control_poller, &pause_signal).await? {
        print_stopped_resume_hint(&job.id, human_stdout_enabled(cli_args.ui));
        return Ok(());
    }
    let double_check_runtime = job_runtime_settings.borrow().clone();
    let double_check_run_config =
        crate::control::freeze_run_config_for_stage(&controlled_run_config, &double_check_runtime);
    if !snapshot.finalize.double_check_complete
        && run_double_check_pass(DoubleCheckPass {
            provider,
            cancel_token,
            cli_args,
            segments,
            translations,
            store,
            job_id: &job.id,
            config: &double_check_run_config,
            settings: &double_check_runtime.settings,
            progress: progress.clone(),
        })
        .await?
    {
        snapshot.finalize.double_check_complete = true;
        store.update_job_config_snapshot(&job.id, snapshot)?;
    }
    if job_was_stopped(store, &job.id)? {
        print_stopped_resume_hint(&job.id, human_stdout_enabled(cli_args.ui));
        return Ok(());
    }

    loop {
        if !wait_for_finalize_stage_control(&mut control_poller, &pause_signal).await? {
            print_stopped_resume_hint(&job.id, human_stdout_enabled(cli_args.ui));
            return Ok(());
        }
        if mark_job_finished(store, &job.id, translations)? {
            break;
        }
        if job_was_stopped(store, &job.id)? {
            print_stopped_resume_hint(&job.id, human_stdout_enabled(cli_args.ui));
            return Ok(());
        }
    }
    let validation_runtime = job_runtime_settings.borrow().clone();
    print_summary_rebuild_and_report(
        store,
        job,
        book,
        segments,
        translations,
        &qa_reviews,
        config,
        rebuild_options,
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

async fn wait_for_finalize_stage_control(
    control: &mut crate::control::ControlFilePoller<'_>,
    signal: &bookforge_llm::PauseSignal,
) -> Result<bool> {
    if let Some(delay_ms) = std::env::var("BOOKFORGE_TEST_FINALIZE_BOUNDARY_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
    }
    Ok(!matches!(
        control.wait_until_running_or_stopped(signal).await?,
        bookforge_llm::PauseState::Stopped
    ))
}

#[derive(Clone)]
pub(crate) struct ProgressRequestProvider<P> {
    inner: P,
    progress: Arc<dyn bookforge_core::ProgressSink>,
    counter: Arc<AtomicUsize>,
    request_prefix: Option<&'static str>,
}

impl<P> ProgressRequestProvider<P> {
    pub(crate) fn new(inner: P, progress: Arc<dyn bookforge_core::ProgressSink>) -> Self {
        Self::new_with_prefix(inner, progress, None)
    }

    pub(crate) fn new_with_prefix(
        inner: P,
        progress: Arc<dyn bookforge_core::ProgressSink>,
        request_prefix: Option<&'static str>,
    ) -> Self {
        Self {
            inner,
            progress,
            counter: Arc::new(AtomicUsize::new(0)),
            request_prefix,
        }
    }
}

impl<P> LlmProvider for ProgressRequestProvider<P>
where
    P: LlmProvider,
{
    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> std::result::Result<CompletionResponse, LlmError> {
        let metadata = request.metadata.clone();
        let max_output_tokens = request.max_output_tokens;
        let prefix = self
            .request_prefix
            .unwrap_or_else(|| finalize_request_prefix(metadata.prompt_template.as_deref()));
        let request_id = format!(
            "{prefix}_{:04}",
            self.counter.fetch_add(1, Ordering::Relaxed)
        );
        self.progress
            .emit(bookforge_core::ProgressEvent::RequestStarted {
                request_id: request_id.clone(),
                batch_id: None,
                segment_id: metadata.segment_id.clone(),
                provider: metadata.provider.clone(),
                model: metadata.model.clone(),
                prompt_template: metadata.prompt_template.clone(),
                items: metadata.block_ids.len().max(1),
                estimated_input_tokens: 0,
                max_output_tokens,
                active_requests: 1,
                target_concurrency: 1,
                runtime_config_revision: metadata.runtime_config_revision,
                provider_max_attempts: metadata.provider_max_attempts,
                timestamp_ms: bookforge_core::progress::now_ms(),
            });

        let started = Instant::now();
        let result = self.inner.complete(request).await;
        let (status, finish_reason, input_tokens, output_tokens, error_kind) = match &result {
            Ok(response) => (
                "ok".to_string(),
                Some(format!("{:?}", response.finish_reason)),
                response.input_tokens,
                response.output_tokens,
                None,
            ),
            Err(error) => (
                "error".to_string(),
                None,
                None,
                None,
                Some(classify_error(error).to_string()),
            ),
        };
        self.progress
            .emit(bookforge_core::ProgressEvent::RequestFinished {
                request_id,
                batch_id: None,
                segment_id: metadata.segment_id,
                status,
                latency_ms: started.elapsed().as_millis() as u64,
                status_code: None,
                finish_reason,
                retry_count: 0,
                input_tokens,
                output_tokens,
                error_kind,
                timestamp_ms: bookforge_core::progress::now_ms(),
            });
        result
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.inner.capabilities()
    }

    fn is_reasoning(&self) -> bool {
        self.inner.is_reasoning()
    }
}

fn finalize_request_prefix(prompt_template: Option<&str>) -> &'static str {
    match prompt_template {
        Some("qa_batch" | "qa_segment") => "qa",
        Some("correct_batch") => "repair",
        Some("double_check_batch") => "double_check",
        _ => "finalize",
    }
}

#[allow(clippy::too_many_arguments)]
fn provider_config(
    provider: &str,
    model: Option<&str>,
    base_url: Option<&str>,
    api_key_env: Option<&str>,
    timeout_seconds: u64,
    provider_max_attempts: usize,
    thinking_disabled: bool,
    retry_after_policy: bookforge_core::RetryAfterPolicy,
    max_backoff_seconds: u64,
    max_idle_per_host: usize,
    json_mode: bookforge_core::JsonMode,
) -> Result<OpenAiCompatibleConfig> {
    let (default_url, default_key_env, default_model) = match provider {
        "deepseek" => (
            "https://api.deepseek.com/v1",
            "DEEPSEEK_API_KEY",
            "deepseek-v4-flash",
        ),
        "openrouter" => (
            "https://openrouter.ai/api/v1",
            "OPENROUTER_API_KEY",
            "openrouter/auto",
        ),
        "openai-compatible" => (
            base_url.ok_or_else(|| {
                anyhow::anyhow!("--base-url is required for --provider openai-compatible")
            })?,
            "OPENAI_API_KEY",
            model.ok_or_else(|| {
                anyhow::anyhow!("--model is required for --provider openai-compatible")
            })?,
        ),
        _ => {
            return Err(anyhow::anyhow!(
                "unsupported translation provider '{provider}'"
            ));
        }
    };

    Ok(OpenAiCompatibleConfig {
        base_url: base_url
            .map(String::from)
            .unwrap_or_else(|| default_url.to_string()),
        api_key_env: api_key_env
            .map(String::from)
            .unwrap_or_else(|| default_key_env.to_string()),
        model: model
            .or(Some(default_model))
            .map(String::from)
            .unwrap_or_else(|| default_model.to_string()),
        timeout_seconds,
        provider_max_attempts: provider_max_attempts.max(1),
        thinking_disabled,
        retry_after_policy,
        max_backoff_seconds,
        max_idle_per_host,
        json_mode,
    })
}

pub(crate) async fn translate_and_checkpoint_batch<P>(
    provider: P,
    segments: &[Segment],
    config: &TranslationRunConfig,
    settings: &ResolvedRunSettings,
    checkpoint: CheckpointContext<'_>,
    progress: Arc<dyn bookforge_core::ProgressSink>,
    mut control: Option<&mut crate::control::ControlFilePoller<'_>>,
) -> Result<Vec<SegmentTranslation>>
where
    P: LlmProvider,
{
    let batches = account_for_batch_prompt_overhead(
        build_translation_batches(segments, &settings.batch, settings.profile),
        &settings.batch,
        config,
    );

    if batches.is_empty() {
        return translate_and_checkpoint(provider, segments, config, checkpoint, control).await;
    }

    eprintln!("Batches: {}", batches.len());

    use std::sync::Arc;
    let telemetry = Arc::new(TelemetryLog::new());

    // Keep the adaptive controller alive even while disabled so a live
    // revision can enable it at the next request boundary without rebuilding
    // or losing its limiter state. The batch engine bypasses it while the
    // current runtime snapshot has adaptive concurrency disabled.
    let rate_controller = {
        let limiter = Arc::new(AdaptiveLimiter::new_with_bounds(
            settings.scheduler.concurrency.max(1),
            1,
            (settings.scheduler.concurrency * 4).max(1),
            std::time::Duration::from_secs(2),
            Some(progress.clone()),
        ));
        Some(Arc::new(ProviderRateController::new(
            limiter,
            RateControllerConfig::for_target(settings.scheduler.concurrency.max(1)),
        )))
    };

    let mut batch_sizer = settings.batch.adaptive_sizing.then(|| {
        bookforge_llm::BatchSizer::with_progress(
            settings.batch.target_tokens,
            settings.batch.max_items,
            progress.clone(),
        )
    });

    let sender = checkpoint.sender.clone();
    let (finalized_tx, mut finalized_rx) = tokio::sync::mpsc::channel::<SegmentTranslation>(64);

    let checkpoint_handle = {
        let sender = sender.clone();
        let job_id = checkpoint.job_id.to_string();
        let provider_name = checkpoint.provider.to_string();
        let model = checkpoint.model.to_string();
        let prompt_version = checkpoint.prompt_version.to_string();
        tokio::spawn(async move {
            while let Some(translation) = finalized_rx.recv().await {
                sender
                    .send(CheckpointCommand::SaveTranslation {
                        job_id: job_id.clone(),
                        translation: Box::new(translation),
                        provider: provider_name.clone(),
                        model: model.clone(),
                        prompt_version: prompt_version.clone(),
                    })
                    .await
                    .map_err(|e| {
                        bookforge_llm::LlmError::Provider(format!("checkpoint send failed: {e}"))
                    })?;
            }
            Ok::<(), bookforge_llm::LlmError>(())
        })
    };

    let batch_result = match control.as_mut() {
        Some(control) => {
            translate_batches_with_control(
                provider,
                batches,
                segments,
                config,
                telemetry.clone(),
                rate_controller,
                batch_sizer.as_mut(),
                progress.clone(),
                Some(finalized_tx),
                |_| Ok(()),
                |signal| {
                    control
                        .poll(signal)
                        .map_err(|err| bookforge_llm::LlmError::Provider(err.to_string()))
                },
            )
            .await
        }
        None => {
            translate_batches_with_callback(
                provider,
                batches,
                segments,
                config,
                telemetry.clone(),
                rate_controller,
                batch_sizer.as_mut(),
                progress.clone(),
                Some(finalized_tx),
                |_| Ok(()),
            )
            .await
        }
    };

    let checkpoint_result = checkpoint_handle.await;

    match (batch_result, checkpoint_result) {
        (Ok(translations), Ok(Ok(()))) => {
            let snapshot = telemetry.snapshot();
            if !snapshot.is_empty() {
                eprintln!("\n{}", telemetry_summary(&snapshot));
            }
            Ok(translations)
        }
        (Err(_), _)
            if config
                .pause_signal
                .as_ref()
                .is_some_and(|signal| signal.is_stopped()) =>
        {
            Ok(Vec::new())
        }
        (Ok(_), Ok(Err(e))) | (Err(e @ bookforge_llm::LlmError::Provider(_)), _) => {
            let message = format!("batch translation checkpoint failure: {e}");
            mark_unfinished_segments_failed(
                checkpoint.store,
                checkpoint.job_id,
                segments,
                &message,
            )?;
            Err(anyhow::anyhow!(message))
        }
        (_, Err(join_err)) => {
            let message = format!("batch checkpoint task panicked: {join_err}");
            mark_unfinished_segments_failed(
                checkpoint.store,
                checkpoint.job_id,
                segments,
                &message,
            )?;
            Err(anyhow::anyhow!(message))
        }
        (Err(error), _) => {
            let message = format!("batch translation failed: {error}");
            mark_unfinished_segments_failed(
                checkpoint.store,
                checkpoint.job_id,
                segments,
                &message,
            )?;
            Err(anyhow::anyhow!(message))
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_fallback_pass(
    cancel_token: &tokio_util::sync::CancellationToken,
    fallback_config: Option<&FallbackPassConfig>,
    segments: &[Segment],
    mut translations: Vec<SegmentTranslation>,
    store: &JobStore,
    job_id: &str,
    prompt_version: &str,
    settings: &ResolvedRunSettings,
    primary_run_config: &TranslationRunConfig,
    control: Option<&mut crate::control::ControlFilePoller<'_>>,
    progress: Arc<dyn bookforge_core::ProgressSink>,
) -> Result<Vec<SegmentTranslation>> {
    let Some(fallback_config) = fallback_config else {
        return Ok(translations);
    };
    let provider_str = fallback_config.provider.as_str();
    let model_str = fallback_config.model.as_str();
    let fallback_scope = fallback_config.scope;
    let fallback_base_url = fallback_config.base_url.as_deref();
    let fallback_api_key_env = fallback_config.api_key_env.as_deref();

    let fallback_status_by_segment = store
        .segment_records(job_id)?
        .into_iter()
        .map(|record| (record.id, record.status))
        .collect::<std::collections::HashMap<_, _>>();
    let candidates: Vec<Segment> = segments
        .iter()
        .filter(|s| {
            let t = translations.iter().find(|t| t.segment_id.0 == s.id.0);
            match t {
                Some(t) => match fallback_scope {
                    FallbackScope::Failed => t.status == SegmentStatus::Failed,
                    FallbackScope::NeedsReview => t.status == SegmentStatus::NeedsReview,
                    FallbackScope::FailedAndNeedsReview => {
                        t.status == SegmentStatus::Failed || t.status == SegmentStatus::NeedsReview
                    }
                },
                None => {
                    let Some(status) = fallback_status_by_segment.get(&s.id.0) else {
                        return false;
                    };
                    match fallback_scope {
                        FallbackScope::Failed => status == "failed",
                        FallbackScope::NeedsReview => status == "needs_review",
                        FallbackScope::FailedAndNeedsReview => {
                            status == "failed" || status == "needs_review"
                        }
                    }
                }
            }
        })
        .cloned()
        .collect();

    if candidates.is_empty() {
        return Ok(translations);
    }

    println!(
        "Fallback: retrying {} segments with {}/{}",
        candidates.len(),
        provider_str,
        model_str
    );

    let run_config = TranslationRunConfig {
        source_language: primary_run_config.source_language.clone(),
        target_language: primary_run_config.target_language.clone(),
        provider: provider_str.to_string(),
        model: model_str.to_string(),
        prompt_version: prompt_version.to_string(),
        temperature: 0.2,
        scheduler: SchedulerConfig {
            concurrency: 1,
            max_attempts: settings.provider.provider_max_attempts,
        },
        profile: settings.profile,
        model_context_tokens: settings.provider.model_context_tokens,
        max_output_tokens: settings.provider.max_output_tokens,
        batch_max_output_tokens: settings.provider.batch_max_output_tokens,
        compact_prompts: settings.compact_prompts,
        glossary: primary_run_config.glossary.clone(),
        context: primary_run_config.context,
        context_registry: primary_run_config.context_registry.clone(),
        style: primary_run_config.style.clone(),
        entities: primary_run_config.entities.clone(),
        pause_signal: Some(primary_run_config.pause_signal.clone().unwrap_or_default()),
        runtime_settings: primary_run_config.runtime_settings.clone(),
    }; // fallback_run_config

    let writer = CheckpointWriter::spawn(store.path().to_path_buf(), Arc::new(NullProgressSink));
    let sender = writer.sender();
    let checkpoint = CheckpointContext {
        store,
        job_id,
        provider: provider_str,
        model: model_str,
        prompt_version,
        sender: &sender,
    };

    let translation_result = match provider_str {
        "mock" => {
            let fallback = MockProvider::new(mock_mode(model_str), &run_config.target_language);
            translate_and_checkpoint(
                ProgressRequestProvider::new_with_prefix(fallback, progress, Some("fallback")),
                &candidates,
                &run_config,
                checkpoint,
                control,
            )
            .await
        }
        "deepseek" | "openrouter" | "openai-compatible" => {
            let provider_config = provider_config(
                provider_str,
                Some(model_str),
                fallback_base_url,
                fallback_api_key_env,
                settings.provider.timeout_seconds,
                settings.provider.provider_max_attempts,
                settings.provider.thinking_disabled,
                settings.provider.retry_after_policy,
                settings.provider.max_backoff_seconds,
                settings.provider.max_idle_per_host,
                settings.provider.json_mode,
            )?;
            let fallback =
                OpenAiCompatibleProvider::new_with_cancel(provider_config, cancel_token.clone())
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
            translate_and_checkpoint(
                ProgressRequestProvider::new_with_prefix(fallback, progress, Some("fallback")),
                &candidates,
                &run_config,
                checkpoint,
                control,
            )
            .await
        }
        provider => anyhow::bail!("unsupported fallback provider '{provider}'"),
    };
    let fresh = finalize_writer(translation_result, sender, writer).await?;

    for ft in &fresh {
        if let Some(existing) = translations
            .iter_mut()
            .find(|t| t.segment_id.0 == ft.segment_id.0)
        {
            *existing = ft.clone();
        } else {
            translations.push(ft.clone());
        }
    }
    translations.sort_by_key(|translation| translation.ordinal);

    Ok(translations)
}

struct DoubleCheckPass<'a> {
    provider: &'a OpenAiCompatibleProvider,
    cancel_token: &'a tokio_util::sync::CancellationToken,
    cli_args: &'a TranslateArgs,
    segments: &'a [Segment],
    translations: &'a mut [SegmentTranslation],
    store: &'a JobStore,
    job_id: &'a str,
    config: &'a TranslationRunConfig,
    settings: &'a ResolvedRunSettings,
    progress: Arc<dyn bookforge_core::ProgressSink>,
}

async fn run_double_check_pass(pass: DoubleCheckPass<'_>) -> Result<bool> {
    let DoubleCheckPass {
        provider,
        cancel_token,
        cli_args,
        segments,
        translations,
        store,
        job_id,
        config,
        settings,
        progress,
    } = pass;
    if settings.double_check.mode == DoubleCheckMode::Off {
        return Ok(true);
    }

    let (dc_provider, dc_provider_name, dc_model) = if cli_args.double_check_provider.is_some()
        || cli_args.double_check_model.is_some()
    {
        let provider_str = cli_args
            .double_check_provider
            .as_deref()
            .unwrap_or("openrouter");
        let dc_config = provider_config(
            provider_str,
            cli_args.double_check_model.as_deref(),
            cli_args.double_check_base_url.as_deref(),
            cli_args.double_check_api_key_env.as_deref(),
            settings.provider.timeout_seconds,
            settings.provider.provider_max_attempts,
            settings.provider.thinking_disabled,
            settings.provider.retry_after_policy,
            settings.provider.max_backoff_seconds,
            settings.provider.max_idle_per_host,
            settings.provider.json_mode,
        )?;
        let provider = OpenAiCompatibleProvider::new_with_cancel(dc_config, cancel_token.clone())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let model = provider.model().to_string();
        (provider, provider_str.to_string(), model)
    } else {
        (
            provider.clone(),
            config.provider.clone(),
            provider.model().to_string(),
        )
    };
    let mut double_check_config = config.clone();
    double_check_config.provider = dc_provider_name;
    double_check_config.model = dc_model;

    println!("Double-check: auditing translations...");
    let corrections = match run_double_check(
        ProgressRequestProvider::new(dc_provider, progress),
        segments,
        translations,
        &double_check_config,
        &settings.double_check,
    )
    .await
    {
        Ok(corrections) => corrections,
        Err(_)
            if config
                .pause_signal
                .as_ref()
                .is_some_and(bookforge_llm::PauseSignal::is_stopped) =>
        {
            return Ok(false);
        }
        Err(e) => return Err(anyhow::anyhow!("double-check failed: {e}")),
    };

    let applied = corrections
        .iter()
        .filter(|c| matches!(c.status, bookforge_llm::CorrectionStatus::Applied))
        .count();
    let rejected = corrections
        .iter()
        .filter(|c| {
            matches!(
                c.status,
                bookforge_llm::CorrectionStatus::RejectedValidationFailed(_)
            )
        })
        .count();
    let unresolved = corrections
        .iter()
        .filter(|c| matches!(c.status, bookforge_llm::CorrectionStatus::Unresolved))
        .count();

    let changed_segment_ids = apply_double_check_corrections(translations, &corrections);
    persist_corrected_translations(store, job_id, config, translations, &changed_segment_ids)?;

    println!(
        "  Corrections: {applied} applied, {rejected} rejected, {unresolved} unresolved, {} segments updated",
        changed_segment_ids.len()
    );

    Ok(true)
}

pub(crate) fn apply_double_check_corrections(
    translations: &mut [SegmentTranslation],
    corrections: &[bookforge_llm::CorrectionRecord],
) -> Vec<String> {
    let mut changed_segment_ids = std::collections::BTreeSet::new();

    for correction in corrections {
        if !matches!(correction.status, bookforge_llm::CorrectionStatus::Applied) {
            continue;
        }
        let Some(corrected) = correction.corrected_translation.as_deref() else {
            continue;
        };
        let Some(translation) = translations
            .iter_mut()
            .find(|translation| translation.segment_id == correction.segment_id)
        else {
            continue;
        };
        let Some(block) = translation
            .blocks
            .iter_mut()
            .find(|block| block.block_id == correction.block_id)
        else {
            continue;
        };
        if block.text != corrected {
            block.text = corrected.to_string();
            changed_segment_ids.insert(translation.segment_id.0.clone());
        }
    }

    changed_segment_ids.into_iter().collect()
}

pub(crate) fn persist_corrected_translations(
    store: &JobStore,
    job_id: &str,
    config: &TranslationRunConfig,
    translations: &[SegmentTranslation],
    changed_segment_ids: &[String],
) -> Result<()> {
    for segment_id in changed_segment_ids {
        let Some(translation) = translations
            .iter()
            .find(|translation| translation.segment_id.0 == *segment_id)
        else {
            continue;
        };
        let joined = translation.joined_text();
        store.save_translation(SaveTranslation {
            job_id,
            segment_id: &translation.segment_id.0,
            translated_text: &joined,
            blocks: &translation.blocks,
            provider: &config.provider,
            model: &config.model,
            prompt_version: &config.prompt_version,
            input_tokens: translation.input_tokens,
            input_cached_tokens: translation.input_cached_tokens,
            output_tokens: translation.output_tokens,
            tokens_estimated: translation.tokens_estimated,
        })?;
    }

    Ok(())
}

#[cfg(test)]
pub(crate) async fn translate_with_scheduler_guard<P>(
    provider: P,
    store: &JobStore,
    job_id: &str,
    segments: &[Segment],
    config: &TranslationRunConfig,
) -> Result<Vec<SegmentTranslation>>
where
    P: LlmProvider,
{
    match translate_segments(provider, segments, config).await {
        Ok(translations) => Ok(translations),
        Err(error) => {
            let message = format!(
                "translation scheduler failed before producing per-segment results: {error}"
            );
            mark_unfinished_segments_failed(store, job_id, segments, &message)?;
            Err(anyhow::anyhow!(message))
        }
    }
}

#[derive(Clone)]
pub(crate) struct CheckpointContext<'a> {
    pub store: &'a JobStore,
    pub job_id: &'a str,
    pub provider: &'a str,
    pub model: &'a str,
    pub prompt_version: &'a str,
    /// Sender to the SQLite writer actor. Per-segment checkpoint writes
    /// go through this channel so the async hot path never blocks on disk.
    pub sender: &'a CheckpointSender,
}

pub(crate) async fn translate_and_checkpoint<P>(
    provider: P,
    segments: &[Segment],
    config: &TranslationRunConfig,
    checkpoint: CheckpointContext<'_>,
    mut control: Option<&mut crate::control::ControlFilePoller<'_>>,
) -> Result<Vec<SegmentTranslation>>
where
    P: LlmProvider,
{
    let sender = checkpoint.sender.clone();
    let (finalized_tx, mut finalized_rx) = tokio::sync::mpsc::channel::<SegmentTranslation>(64);

    // Spawn a task that checkpoints each finalized translation as it arrives.
    let checkpoint_handle = {
        let sender = sender.clone();
        let job_id = checkpoint.job_id.to_string();
        let provider_name = checkpoint.provider.to_string();
        let model = checkpoint.model.to_string();
        let prompt_version = checkpoint.prompt_version.to_string();
        tokio::spawn(async move {
            while let Some(translation) = finalized_rx.recv().await {
                sender
                    .send(CheckpointCommand::SaveTranslation {
                        job_id: job_id.clone(),
                        translation: Box::new(translation),
                        provider: provider_name.clone(),
                        model: model.clone(),
                        prompt_version: prompt_version.clone(),
                    })
                    .await
                    .map_err(|e| {
                        bookforge_llm::LlmError::Provider(format!("checkpoint send failed: {e}"))
                    })?;
            }
            Ok::<(), bookforge_llm::LlmError>(())
        })
    };

    let translations = match control.as_mut() {
        Some(control) => {
            translate_segments_with_control(
                provider,
                segments,
                config,
                |_| Ok(()),
                Some(finalized_tx),
                |signal| {
                    control
                        .poll(signal)
                        .map_err(|err| bookforge_llm::LlmError::Provider(err.to_string()))
                },
            )
            .await
        }
        None => {
            translate_segments_with_callback(
                provider,
                segments,
                config,
                |_| Ok(()),
                Some(finalized_tx),
            )
            .await
        }
    };

    // Drop finalized_tx so the checkpoint task exits
    // (finalized_tx was moved into translate_segments_with_callback)
    let checkpoint_result = checkpoint_handle.await;

    match (translations, checkpoint_result) {
        (Ok(translations), Ok(Ok(()))) => Ok(translations),
        (Err(_), _)
            if config
                .pause_signal
                .as_ref()
                .is_some_and(|signal| signal.is_stopped()) =>
        {
            Ok(Vec::new())
        }
        (Ok(_), Ok(Err(e))) | (Err(e @ bookforge_llm::LlmError::Provider(_)), _) => {
            let message = format!("translation checkpoint failure: {e}");
            mark_unfinished_segments_failed(
                checkpoint.store,
                checkpoint.job_id,
                segments,
                &message,
            )?;
            Err(anyhow::anyhow!(message))
        }
        (_, Err(join_err)) => {
            let message = format!("checkpoint task panicked: {join_err}");
            mark_unfinished_segments_failed(
                checkpoint.store,
                checkpoint.job_id,
                segments,
                &message,
            )?;
            Err(anyhow::anyhow!(message))
        }
        (Err(error), _) => {
            let message = format!(
                "translation scheduler failed before producing per-segment results: {error}"
            );
            mark_unfinished_segments_failed(
                checkpoint.store,
                checkpoint.job_id,
                segments,
                &message,
            )?;
            Err(anyhow::anyhow!(message))
        }
    }
}

pub(crate) async fn qa_reviews_for_mode<P>(
    provider: P,
    segments: &[Segment],
    translations: &[SegmentTranslation],
    config: &TranslationRunConfig,
    qa_config: &bookforge_core::config::QaRunConfig,
    qa_mode: QaMode,
) -> Vec<QaSegmentReview>
where
    P: LlmProvider,
{
    match qa_mode {
        QaMode::Off => Vec::new(),
        QaMode::All => {
            qa_segments_parallel(provider, segments, translations, config, qa_config).await
        }
        QaMode::Suspicious => {
            let candidates = suspicious_qa_candidates(segments, translations);
            qa_segments_parallel(provider, segments, &candidates, config, qa_config).await
        }
    }
}

fn suspicious_qa_candidates(
    segments: &[Segment],
    translations: &[SegmentTranslation],
) -> Vec<SegmentTranslation> {
    let by_segment = segments
        .iter()
        .map(|segment| (segment.id.0.as_str(), segment))
        .collect::<std::collections::HashMap<_, _>>();
    translations
        .iter()
        .filter(|translation| {
            matches!(
                translation.status,
                SegmentStatus::Succeeded | SegmentStatus::SkippedCached
            )
        })
        .filter(|translation| {
            let Some(segment) = by_segment.get(translation.segment_id.0.as_str()) else {
                return false;
            };
            let source_len = segment.source.text.chars().count().max(1);
            let translated_len = translation.joined_text().chars().count();
            let ratio = translated_len as f64 / source_len as f64;
            !(0.5..=2.2).contains(&ratio)
                || translation.template == "translate_run_preserving"
                || segment.constraints.preserve_spans.len() >= 4
                || marker_structure_changed(segment, translation)
        })
        .cloned()
        .collect()
}

#[derive(Debug, Clone, Eq, Ord, PartialEq, PartialOrd)]
struct MarkerSignature {
    block_index: usize,
    id: String,
    shape: MarkerShape,
}

#[derive(Debug, Clone, Eq, Ord, PartialEq, PartialOrd)]
enum MarkerShape {
    PairedM,
    PairedKeep,
    EmptyRef,
}

fn marker_structure_changed(segment: &Segment, translation: &SegmentTranslation) -> bool {
    let Some(mut expected) = marker_signatures_for_blocks(
        segment
            .source
            .blocks
            .iter()
            .map(|block| block.text.as_str()),
    ) else {
        return true;
    };
    let Some(mut actual) =
        marker_signatures_for_blocks(translation.blocks.iter().map(|block| block.text.as_str()))
    else {
        return true;
    };
    expected.sort();
    actual.sort();
    expected != actual
}

fn marker_signatures_for_blocks<'a>(
    blocks: impl Iterator<Item = &'a str>,
) -> Option<Vec<MarkerSignature>> {
    let mut signatures = Vec::new();
    for (block_index, text) in blocks.enumerate() {
        signatures.extend(marker_signatures_in_text(block_index, text)?);
    }
    Some(signatures)
}

fn marker_signatures_in_text(block_index: usize, text: &str) -> Option<Vec<MarkerSignature>> {
    let mut signatures = Vec::new();
    let mut open_stack: Vec<String> = Vec::new();
    let mut rest = text;

    while let Some(index) = rest.find('<') {
        let tag = &rest[index..];
        if let Some(open) = parse_paired_marker_open(tag) {
            let shape = if open.tag_name == "keep" {
                MarkerShape::PairedKeep
            } else {
                MarkerShape::PairedM
            };
            signatures.push(MarkerSignature {
                block_index,
                id: open.id,
                shape,
            });
            open_stack.push(open.tag_name);
            rest = &tag[open.len..];
        } else if let Some(empty) = parse_empty_marker(tag) {
            signatures.push(MarkerSignature {
                block_index,
                id: empty.id,
                shape: MarkerShape::EmptyRef,
            });
            rest = &tag[empty.len..];
        } else if let Some(close) = parse_marker_close(tag) {
            if open_stack.pop().as_deref() != Some(close.tag_name.as_str()) {
                return None;
            }
            rest = &tag[close.len..];
        } else {
            rest = &tag[1..];
        }
    }

    if open_stack.is_empty() {
        Some(signatures)
    } else {
        None
    }
}

fn mark_unfinished_segments_failed(
    store: &JobStore,
    job_id: &str,
    segments: &[Segment],
    error: &str,
) -> Result<()> {
    let segment_ids = segments
        .iter()
        .map(|segment| segment.id.0.clone())
        .collect::<Vec<_>>();
    store.mark_unfinished_segments_failed(job_id, &segment_ids, error)?;
    Ok(())
}

pub(crate) fn mark_job_finished(
    store: &JobStore,
    job_id: &str,
    translations: &[SegmentTranslation],
) -> Result<bool> {
    if job_was_stopped(store, job_id)? || job_is_paused(store, job_id)? {
        return Ok(false);
    }
    let Some(summary) = store.summary(job_id)? else {
        anyhow::bail!("job '{job_id}' was not found");
    };
    let terminal_segments =
        summary.succeeded + summary.cached + summary.failed + summary.needs_review;
    if terminal_segments < summary.total_segments || summary.retry_pending > 0 {
        store.mark_job_needs_review(job_id)?;
        return Ok(!job_was_stopped(store, job_id)? && !job_is_paused(store, job_id)?);
    }
    if translations
        .iter()
        .any(|translation| translation.status == SegmentStatus::Failed)
    {
        store.mark_job_needs_review(job_id)?;
        return Ok(!job_was_stopped(store, job_id)? && !job_is_paused(store, job_id)?);
    }
    if translations
        .iter()
        .any(|translation| translation.status == SegmentStatus::NeedsReview)
    {
        store.mark_job_needs_review(job_id)?;
        return Ok(!job_was_stopped(store, job_id)? && !job_is_paused(store, job_id)?);
    }
    store.mark_job_complete(job_id)?;
    Ok(!job_was_stopped(store, job_id)? && !job_is_paused(store, job_id)?)
}

pub(crate) fn job_was_stopped(store: &JobStore, job_id: &str) -> Result<bool> {
    Ok(store
        .get_job(job_id)?
        .is_some_and(|job| job.status == "stopped"))
}

pub(crate) fn job_is_paused(store: &JobStore, job_id: &str) -> Result<bool> {
    Ok(store
        .get_job(job_id)?
        .is_some_and(|job| job.status == "paused"))
}

pub(crate) fn print_stopped_resume_hint(job_id: &str, print_stdout: bool) {
    if print_stdout {
        println!("Stopped. Progress has been saved to job: {job_id}");
        println!("Resume with: bookforge resume {job_id}");
    }
}

#[derive(Debug, Args)]
pub struct BenchmarkArgs {
    #[command(flatten)]
    pub provider: CliProviderArgs,

    #[arg(long, default_value_t = 5)]
    pub samples: usize,

    #[arg(long, default_value_t = 1000)]
    pub tokens: usize,

    #[arg(long, default_value_t = 1)]
    pub concurrency: usize,
}

pub async fn run_benchmark(args: BenchmarkArgs) -> Result<()> {
    let pigeon = "Sunt piger, et volare nequeunt. Sed cum cibus apparet, mirabiliter currunt.";
    let provider_config = OpenAiCompatibleConfig {
        base_url: args
            .provider
            .base_url
            .clone()
            .unwrap_or_else(|| "https://openrouter.ai/api/v1".to_string()),
        api_key_env: args
            .provider
            .api_key_env
            .clone()
            .unwrap_or_else(|| "OPENROUTER_API_KEY".to_string()),
        model: args
            .provider
            .model
            .clone()
            .unwrap_or_else(|| "openrouter/auto".to_string()),
        timeout_seconds: args.provider.timeout_seconds.unwrap_or(120),
        provider_max_attempts: 6,
        thinking_disabled: false,
        retry_after_policy: bookforge_core::RetryAfterPolicy::JitteredExponential,
        max_backoff_seconds: 30,
        max_idle_per_host: 32,
        json_mode: bookforge_core::JsonMode::Auto,
    }; // benchmark

    let provider = OpenAiCompatibleProvider::new(provider_config.clone())?;
    let model = provider.model().to_string();

    println!("Benchmarking {} / {}", provider_config.base_url, model);
    println!(
        "Samples: {}, Tokens: {}, Concurrency: {}",
        args.samples, args.tokens, args.concurrency
    );
    println!();

    let mut latencies = Vec::with_capacity(args.samples);
    let mut success_count = 0usize;
    let mut failure_count = 0usize;
    let mut ratelimit_count = 0usize;
    let mut timeout_count = 0usize;
    let mut total_output_tokens = 0u64;
    let mut _total_input_tokens = 0u64;

    for i in 0..args.samples {
        let request = bookforge_llm::CompletionRequest {
            system: "You are a translator. Return JSON only: {\"translation\":\"...\"}".to_string(),
            user: format!("Translate: {{\"text\":\"{}\"}} Return JSON.", pigeon),
            response_format: bookforge_llm::ResponseFormat::Json,
            temperature: 0.2,
            max_output_tokens: Some(args.tokens as u32),
            metadata: Default::default(),
        };

        print!("  [{}/{}] ", i + 1, args.samples);
        match provider.complete(request).await {
            Ok(resp) => {
                latencies.push(resp.provider_latency_ms);
                success_count += 1;
                total_output_tokens += resp.output_tokens.unwrap_or(0);
                _total_input_tokens += resp.input_tokens.unwrap_or(0);
                let tok_sec = if resp.provider_latency_ms > 0 {
                    resp.output_tokens.unwrap_or(0) as f64
                        / (resp.provider_latency_ms as f64 / 1000.0)
                } else {
                    0.0
                };
                println!(
                    "OK {}ms finish={:?} in={:?} out={:?} ~{tok_sec:.0}tok/s",
                    resp.provider_latency_ms,
                    resp.finish_reason,
                    resp.input_tokens,
                    resp.output_tokens
                );
            }
            Err(e) => {
                failure_count += 1;
                let kind = classify_error(&e);
                match kind {
                    "rate_limit" => ratelimit_count += 1,
                    "timeout" => timeout_count += 1,
                    _ => {}
                }
                println!("FAIL [{kind}] {e}");
            }
        }
    }

    println!();
    println!("Results:");
    println!("  Success: {} / {}", success_count, args.samples);
    println!("  Failed:  {}", failure_count);

    if !latencies.is_empty() {
        latencies.sort();
        let p50 = percentile(&latencies, 50);
        let p95 = percentile(&latencies, 95);
        let avg = latencies.iter().sum::<u64>() as f64 / latencies.len() as f64;
        let avg_tok_sec = if avg > 0.0 {
            total_output_tokens as f64 / (avg * latencies.len() as f64 / 1000.0)
        } else {
            0.0
        };

        println!("  p50 latency: {}ms", p50);
        println!("  p95 latency: {}ms", p95);
        println!("  avg latency:  {:.0}ms", avg);
        println!("  avg output:   {:.0} tok/s", avg_tok_sec);
    }

    println!("  429 count:    {}", ratelimit_count);
    println!("  timeout count: {}", timeout_count);

    if !latencies.is_empty() {
        let p50 = percentile(&latencies, 50);
        let recommendation = if ratelimit_count > 0 || p50 > 120_000 {
            ("free-tier", 1usize, 300u64)
        } else if p50 < 15_000 && ratelimit_count == 0 {
            ("fastest", 32usize, 120u64)
        } else {
            ("balanced", 16usize, 120u64)
        };
        println!();
        println!("Recommendation:");
        println!("  profile:     {}", recommendation.0);
        println!("  concurrency: {}", recommendation.1);
        println!("  timeout:     {}s", recommendation.2);
    }

    Ok(())
}

fn classify_error(e: &LlmError) -> &'static str {
    match e {
        LlmError::Http(http_err) => {
            if http_err.is_timeout() {
                "timeout"
            } else {
                "http"
            }
        }
        LlmError::HttpStatus { status, .. } if *status == 429 => "rate_limit",
        LlmError::HttpStatus { status, .. } if (500..600).contains(status) => "server",
        LlmError::HttpStatus { .. } => "client",
        LlmError::Provider(_) => "provider",
        LlmError::InvalidResponse(_) => "invalid_response",
        LlmError::Json(_) => "json",
    }
}

fn percentile(data: &[u64], pct: usize) -> u64 {
    if data.is_empty() {
        return 0;
    }
    let idx = ((pct as f64 / 100.0) * (data.len() - 1) as f64).round() as usize;
    data[idx.min(data.len() - 1)]
}

#[cfg(test)]
mod tests;
