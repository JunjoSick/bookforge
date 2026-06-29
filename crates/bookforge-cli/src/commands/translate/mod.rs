use anyhow::Result;
#[cfg(test)]
use bookforge_core::config::TranslationProfile;
use bookforge_core::{
    GlossaryFormat, GlossaryTerm, NullProgressSink,
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
    AdaptiveLimiter, ContextRegistry, ContextRunConfig, EntityRunConfig, GlossaryRunConfig,
    LlmError, LlmProvider, MockMode, MockProvider, OpenAiCompatibleConfig,
    OpenAiCompatibleProvider, ProviderRateController, QaSegmentReview, RateControllerConfig,
    SegmentTranslation, StyleRunConfig, TelemetryLog, TranslationRunConfig,
    account_for_batch_prompt_overhead, build_translation_batches, qa_segments_parallel,
    run_double_check, telemetry_summary, translate_batches_with_callback,
    translate_segments_with_callback,
};
use bookforge_store::{CreateJob, JobRecord, JobStore, SaveTranslation};
use clap::Args;
use sha2::{Digest, Sha256};
use std::{path::PathBuf, sync::Arc};

#[cfg(test)]
use crate::LanguageArgs;
use crate::{
    ProviderArgs as CliProviderArgs, QaMode,
    checkpoint::{CheckpointCommand, CheckpointSender, CheckpointWriter},
    commands::glossary::read_glossary_file,
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
use settings::{apply_provider_preset, resolve_settings, retry_amplification_warning};
use snapshot::persist_snapshot;

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
    persist_snapshot(
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
    store.insert_segments(
        &job.id,
        &segments,
        prompt_version,
        "mock",
        &model,
        &cache_namespace,
    )?;
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
        false,
    )
    .await?;
    translations.extend(fresh_translations);
    translations.sort_by_key(|translation| translation.ordinal);
    let qa_reviews = qa_reviews_for_mode(
        provider,
        &segments,
        &translations,
        &run_config,
        &settings.qa,
        cli_args.qa,
    )
    .await;
    mark_job_finished(&store, &job.id, &translations)?;
    print_summary_rebuild_and_report(
        &store,
        &job,
        &book,
        &segments,
        &translations,
        &qa_reviews,
        config,
        cli_args.validate_output,
        cli_args.strict_epubcheck,
        human_stdout_enabled(cli_args.ui),
    )?;
    let summary = store
        .summary(&job.id)?
        .ok_or_else(|| anyhow::anyhow!("job '{}' summary unavailable", job.id))?;
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
    let mut provider_config = match provider_config(
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
    ) {
        Ok(cfg) => cfg,
        Err(e) => {
            if config.provider == "openai-compatible" {
                return Err(anyhow::anyhow!(
                    "--base-url is required for --provider openai-compatible"
                ));
            }
            return Err(e);
        }
    };
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
        PromptVersion::BatchV2.as_str()
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
    persist_snapshot(
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
    store.insert_segments(
        &job.id,
        &segments,
        run_prompt_version,
        &config.provider,
        &model,
        &cache_namespace,
    )?;
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
        &book,
        progress.clone(),
        started,
    )
    .await?;

    if cancel_token.is_cancelled() {
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
    book: &bookforge_core::ir::Book,
    progress: Arc<dyn bookforge_core::ProgressSink>,
    started: std::time::Instant,
) -> Result<()> {
    translations.sort_by_key(|t| t.ordinal);

    let qa_reviews = qa_reviews_for_mode(
        provider.clone(),
        segments,
        translations,
        run_config,
        &settings.qa,
        cli_args.qa,
    )
    .await;

    let fallback_translations = run_fallback_pass(
        provider,
        cli_args,
        segments,
        std::mem::take(translations),
        store,
        &job.id,
        run_prompt_version,
        settings,
        run_config,
    )
    .await?;
    *translations = fallback_translations;

    run_double_check_pass(DoubleCheckPass {
        provider,
        cancel_token,
        cli_args,
        segments,
        translations,
        store,
        job_id: &job.id,
        config: run_config,
        settings,
    })
    .await?;

    mark_job_finished(store, &job.id, translations)?;
    print_summary_rebuild_and_report(
        store,
        job,
        book,
        segments,
        translations,
        &qa_reviews,
        config,
        cli_args.validate_output,
        cli_args.strict_epubcheck,
        human_stdout_enabled(cli_args.ui),
    )?;
    let summary = store
        .summary(&job.id)?
        .ok_or_else(|| anyhow::anyhow!("job '{}' summary unavailable", job.id))?;
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
        _ => {
            return Err(anyhow::anyhow!(
                "--base-url is required for --provider {provider}"
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
        return translate_and_checkpoint(provider, segments, config, checkpoint).await;
    }

    eprintln!("Batches: {}", batches.len());

    use std::sync::Arc;
    let telemetry = Arc::new(TelemetryLog::new());

    let rate_controller = if settings.adaptive_concurrency {
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
    } else {
        None
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

    let batch_result = translate_batches_with_callback(
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
    .await;

    let checkpoint_result = checkpoint_handle.await;

    match (batch_result, checkpoint_result) {
        (Ok(translations), Ok(Ok(()))) => {
            let snapshot = telemetry.snapshot();
            if !snapshot.is_empty() {
                eprintln!("\n{}", telemetry_summary(&snapshot));
            }
            Ok(translations)
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
async fn run_fallback_pass(
    primary_provider: &OpenAiCompatibleProvider,
    cli_args: &TranslateArgs,
    segments: &[Segment],
    mut translations: Vec<SegmentTranslation>,
    store: &JobStore,
    job_id: &str,
    prompt_version: &str,
    settings: &ResolvedRunSettings,
    primary_run_config: &TranslationRunConfig,
) -> Result<Vec<SegmentTranslation>> {
    if cli_args.fallback_provider.is_none() && cli_args.fallback_model.is_none() {
        return Ok(translations);
    }

    let provider_str = cli_args
        .fallback_provider
        .as_deref()
        .unwrap_or("openrouter");
    let model_str = cli_args
        .fallback_model
        .as_deref()
        .unwrap_or(primary_provider.model());

    let fallback_config = provider_config(
        provider_str,
        Some(model_str),
        cli_args.fallback_base_url.as_deref(),
        cli_args.fallback_api_key_env.as_deref(),
        settings.provider.timeout_seconds,
        settings.provider.provider_max_attempts,
        settings.provider.thinking_disabled,
        settings.provider.retry_after_policy,
        settings.provider.max_backoff_seconds,
        settings.provider.max_idle_per_host,
        settings.provider.json_mode,
    )?;

    let fallback = OpenAiCompatibleProvider::new_with_cancel(
        fallback_config,
        primary_provider.cancel_token.clone(),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    let fallback_model = fallback.model().to_string();

    let candidates: Vec<Segment> = segments
        .iter()
        .filter(|s| {
            let t = translations.iter().find(|t| t.segment_id.0 == s.id.0);
            match t {
                Some(t) => match cli_args.fallback_only {
                    FallbackScope::Failed => t.status == SegmentStatus::Failed,
                    FallbackScope::NeedsReview => t.status == SegmentStatus::NeedsReview,
                    FallbackScope::FailedAndNeedsReview => {
                        t.status == SegmentStatus::Failed || t.status == SegmentStatus::NeedsReview
                    }
                },
                None => false,
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
        fallback_model
    );

    let run_config = TranslationRunConfig {
        source_language: primary_run_config.source_language.clone(),
        target_language: primary_run_config.target_language.clone(),
        provider: provider_str.to_string(),
        model: fallback_model.clone(),
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
    }; // fallback_run_config

    let writer = CheckpointWriter::spawn(store.path().to_path_buf(), Arc::new(NullProgressSink));
    let sender = writer.sender();
    let checkpoint = CheckpointContext {
        store,
        job_id,
        provider: provider_str,
        model: &fallback_model,
        prompt_version,
        sender: &sender,
    };

    let translation_result =
        translate_and_checkpoint(fallback, &candidates, &run_config, checkpoint).await;
    let fresh = finalize_writer(translation_result, sender, writer).await?;

    for ft in &fresh {
        if let Some(existing) = translations
            .iter_mut()
            .find(|t| t.segment_id.0 == ft.segment_id.0)
        {
            *existing = ft.clone();
        }
    }

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
}

async fn run_double_check_pass(pass: DoubleCheckPass<'_>) -> Result<()> {
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
    } = pass;
    if settings.double_check.mode == DoubleCheckMode::Off {
        return Ok(());
    }

    let dc_provider =
        if cli_args.double_check_provider.is_some() || cli_args.double_check_model.is_some() {
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
            OpenAiCompatibleProvider::new_with_cancel(dc_config, cancel_token.clone())
                .map_err(|e| anyhow::anyhow!("{e}"))?
        } else {
            provider.clone()
        };

    println!("Double-check: auditing translations...");
    let corrections = run_double_check(
        dc_provider,
        segments,
        translations,
        config,
        &settings.double_check,
    )
    .await
    .map_err(|e| anyhow::anyhow!("double-check failed: {e}"))?;

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

    Ok(())
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

fn persist_corrected_translations(
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

    let translations = translate_segments_with_callback(
        provider,
        segments,
        config,
        |_| Ok(()),
        Some(finalized_tx),
    )
    .await;

    // Drop finalized_tx so the checkpoint task exits
    // (finalized_tx was moved into translate_segments_with_callback)
    let checkpoint_result = checkpoint_handle.await;

    match (translations, checkpoint_result) {
        (Ok(translations), Ok(Ok(()))) => Ok(translations),
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
) -> Result<()> {
    let Some(summary) = store.summary(job_id)? else {
        anyhow::bail!("job '{job_id}' was not found");
    };
    let terminal_segments =
        summary.succeeded + summary.cached + summary.failed + summary.needs_review;
    if terminal_segments < summary.total_segments || summary.retry_pending > 0 {
        store.mark_job_needs_review(job_id)?;
        return Ok(());
    }
    if translations
        .iter()
        .any(|translation| translation.status == SegmentStatus::Failed)
    {
        store.mark_job_needs_review(job_id)?;
        return Ok(());
    }
    if translations
        .iter()
        .any(|translation| translation.status == SegmentStatus::NeedsReview)
    {
        store.mark_job_needs_review(job_id)?;
        return Ok(());
    }
    store.mark_job_complete(job_id)?;
    Ok(())
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
mod tests {
    use super::*;
    use bookforge_core::{
        ir::{BlockId, SectionId},
        segment::{
            BlockTranslation, SegmentBlock, SegmentConstraints, SegmentContext, SegmentId,
            SegmentMetadata, SegmentSource, SegmentTextRun,
        },
    };
    use std::{fs, time::SystemTime};

    #[tokio::test]
    async fn scheduler_guard_preserves_completed_segments_on_run_level_error() {
        let db_path = temp_path("jobs.sqlite");
        let input_path = temp_path("input.epub");
        fs::write(&input_path, b"epub bytes").expect("input fixture should be writable");

        let store = JobStore::open(&db_path).expect("store should open");
        let job = store
            .create_job(CreateJob {
                input: &input_path,
                output: &temp_path("output.epub"),
                source_lang: Some("English"),
                target_lang: "Italian",
                provider: "mock",
                model: "mock-prefix",
                base_url: None,
                api_key_env: None,
                book_id: None,
                series_id: None,
            })
            .expect("job should be created");
        let segments = vec![segment("seg_a", 0), segment("seg_b", 1)];
        store
            .insert_segments(&job.id, &segments, "v1", "mock", "mock-prefix", "test_ns")
            .expect("segments should insert");
        store
            .save_translation(bookforge_store::SaveTranslation {
                job_id: &job.id,
                segment_id: "seg_a",
                translated_text: "Gia fatto",
                blocks: &[],
                input_tokens: Some(1),
                input_cached_tokens: Some(0),
                output_tokens: Some(2),
                tokens_estimated: false,
                provider: "mock",
                model: "mock-prefix",
                prompt_version: "v1",
            })
            .expect("completed segment should save");
        let config = TranslationRunConfig {
            source_language: Some("English".to_string()),
            target_language: "Italian".to_string(),
            provider: "mock".to_string(),
            model: "mock-prefix".to_string(),
            prompt_version: "v1".to_string(),
            temperature: 0.2,
            scheduler: SchedulerConfig {
                concurrency: 0,
                max_attempts: 1,
            },
            profile: TranslationProfile::Balanced,
            model_context_tokens: None,
            max_output_tokens: None,
            batch_max_output_tokens: None,
            compact_prompts: false,
            glossary: GlossaryRunConfig::default(),
            context: ContextRunConfig::default(),
            context_registry: None,
            style: None,
            entities: None,
        };

        let error = translate_with_scheduler_guard(
            MockProvider::new(MockMode::PrefixTarget, "Italian"),
            &store,
            &job.id,
            &segments,
            &config,
        )
        .await
        .expect_err("zero concurrency is a scheduler-level error");

        assert!(
            error
                .to_string()
                .contains("before producing per-segment results")
        );
        let summary = store
            .summary(&job.id)
            .expect("summary should load")
            .expect("job should exist");
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.succeeded, 1);

        let _ = fs::remove_file(db_path);
        let _ = fs::remove_file(input_path);
    }

    #[test]
    fn mark_job_finished_keeps_queued_segments_needs_review() {
        let db_path = temp_path("queued_finish.sqlite");
        let input_path = temp_path("queued_finish_input.epub");
        fs::write(&input_path, b"epub bytes").expect("input fixture should be writable");

        let store = JobStore::open(&db_path).expect("store should open");
        let job = store
            .create_job(CreateJob {
                input: &input_path,
                output: &temp_path("queued_finish_output.epub"),
                source_lang: Some("English"),
                target_lang: "Italian",
                provider: "mock",
                model: "mock-prefix",
                base_url: None,
                api_key_env: None,
                book_id: None,
                series_id: None,
            })
            .expect("job should be created");
        let segments = vec![segment("seg_a", 0), segment("seg_b", 1)];
        store
            .insert_segments(&job.id, &segments, "v1", "mock", "mock-prefix", "test_ns")
            .expect("segments should insert");
        let translation = translation_for(
            &segments[0],
            "Gia fatto",
            "translate_segment",
            SegmentStatus::Succeeded,
        );
        store
            .save_translation(bookforge_store::SaveTranslation {
                job_id: &job.id,
                segment_id: &translation.segment_id.0,
                translated_text: &translation.joined_text(),
                blocks: &translation.blocks,
                input_tokens: Some(1),
                input_cached_tokens: Some(0),
                output_tokens: Some(2),
                tokens_estimated: false,
                provider: "mock",
                model: "mock-prefix",
                prompt_version: "v1",
            })
            .expect("completed segment should save");

        mark_job_finished(&store, &job.id, &[translation]).expect("job finish should succeed");

        let job = store
            .get_job(&job.id)
            .expect("job should load")
            .expect("job should exist");
        assert_eq!(job.status, "needs_review");

        let _ = fs::remove_file(db_path);
        let _ = fs::remove_file(input_path);
    }

    #[test]
    fn glossary_file_is_selected_for_matching_segment() {
        let db_path = temp_path("glossary_prepare.sqlite");
        let glossary_path = temp_path("glossary.toml");
        fs::write(
            &glossary_path,
            r#"
[meta]
schema_version = 1
source_language = "English"
target_language = "Italian"

[meta.scope]
kind = "book"
id = "fellowship"

[[term]]
source = "Aragorn"
target = "Aragorn"
category = "person"
case_sensitive = true
"#,
        )
        .expect("glossary fixture should write");
        let store = JobStore::open(&db_path).expect("store should open");
        let mut segment = segment("seg_a", 0);
        segment.source.text = "Aragorn entered the room.".to_string();
        segment.source.blocks[0].text = segment.source.text.clone();

        let prepared = prepare_glossary_run_config(
            &store,
            std::slice::from_ref(&glossary_path),
            Some("English"),
            "Italian",
            Some("fellowship"),
            None,
            GlossaryFormat::Json,
            800,
            None,
            &[segment],
        )
        .expect("glossary should prepare");

        let entries = &prepared.run_config.entries_by_segment["seg_a"];
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source, "Aragorn");
        assert_eq!(entries[0].target, "Aragorn");

        let _ = fs::remove_file(db_path);
        let _ = fs::remove_file(glossary_path);
    }

    #[test]
    fn persisted_glossary_is_selected_when_source_is_auto() {
        let db_path = temp_path("glossary_auto_source.sqlite");
        let store = JobStore::open(&db_path).expect("store should open");
        store
            .upsert_glossary_terms(&[GlossaryTerm {
                id: None,
                scope_kind: bookforge_core::GlossaryScopeKind::Book,
                scope_id: Some("fellowship".to_string()),
                source_text: "Aragorn".to_string(),
                target_text: "Aragorn".to_string(),
                category: bookforge_core::GlossaryCategory::Person,
                notes: None,
                case_sensitive: true,
                always_active: false,
                status: bookforge_core::GlossaryStatus::UserSeeded,
                source_language: "English".to_string(),
                target_language: "Italian".to_string(),
                source_count: 0,
            }])
            .expect("persisted glossary should insert");
        let mut segment = segment("seg_auto", 0);
        segment.source.text = "Aragorn entered the room.".to_string();
        segment.source.blocks[0].text = segment.source.text.clone();

        let prepared = prepare_glossary_run_config(
            &store,
            &[],
            None,
            "Italian",
            Some("fellowship"),
            None,
            GlossaryFormat::Json,
            800,
            None,
            &[segment],
        )
        .expect("glossary should prepare without explicit source");

        let entries = &prepared.run_config.entries_by_segment["seg_auto"];
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source, "Aragorn");
        assert_eq!(entries[0].target, "Aragorn");

        let _ = fs::remove_file(db_path);
    }

    #[test]
    fn glossary_format_changes_cache_fingerprint() {
        let term = GlossaryTerm {
            id: Some(1),
            scope_kind: bookforge_core::GlossaryScopeKind::Book,
            scope_id: Some("fellowship".to_string()),
            source_text: "Aragorn".to_string(),
            target_text: "Aragorn".to_string(),
            category: bookforge_core::GlossaryCategory::Person,
            notes: None,
            case_sensitive: true,
            always_active: false,
            status: bookforge_core::GlossaryStatus::UserSeeded,
            source_language: "English".to_string(),
            target_language: "Italian".to_string(),
            source_count: 0,
        };

        let json =
            glossary_fingerprint(GlossaryFormat::Json, 800, None, std::slice::from_ref(&term));
        let prose = glossary_fingerprint(GlossaryFormat::Prose, 800, None, &[term]);

        assert_ne!(json, prose);
    }

    #[test]
    fn applied_double_check_corrections_update_matching_blocks() {
        let mut translations = vec![SegmentTranslation {
            segment_id: SegmentId("seg_a".to_string()),
            ordinal: 0,
            block_ids: vec![BlockId("b_000000".to_string())],
            blocks: vec![BlockTranslation {
                block_id: BlockId("b_000000".to_string()),
                text: "vecchio testo".to_string(),
            }],
            checksum: "checksum".to_string(),
            status: SegmentStatus::Succeeded,
            template: "translate_segment".to_string(),
            error: None,
            input_tokens: Some(10),
            input_cached_tokens: Some(0),
            output_tokens: Some(12),
            tokens_estimated: false,
        }];
        let corrections = vec![
            bookforge_llm::CorrectionRecord {
                item_id: "seg_a:b_000000".to_string(),
                segment_id: SegmentId("seg_a".to_string()),
                block_id: BlockId("b_000000".to_string()),
                original_translation: "vecchio testo".to_string(),
                corrected_translation: Some("testo corretto".to_string()),
                status: bookforge_llm::CorrectionStatus::Applied,
                issues: Vec::new(),
            },
            bookforge_llm::CorrectionRecord {
                item_id: "seg_a:b_000000".to_string(),
                segment_id: SegmentId("seg_a".to_string()),
                block_id: BlockId("b_000000".to_string()),
                original_translation: "testo corretto".to_string(),
                corrected_translation: Some("non applicato".to_string()),
                status: bookforge_llm::CorrectionStatus::Unresolved,
                issues: Vec::new(),
            },
        ];

        let changed = apply_double_check_corrections(&mut translations, &corrections);

        assert_eq!(changed, vec!["seg_a".to_string()]);
        assert_eq!(translations[0].blocks[0].text, "testo corretto");
    }

    #[test]
    fn suspicious_qa_ignores_matching_inline_markers() {
        let segment = marked_segment("seg_marker", 0, "<m1>Hello</m1>");
        let translation = translation_for(
            &segment,
            "<m1>Ciao</m1>",
            "stored",
            SegmentStatus::Succeeded,
        );

        let candidates = suspicious_qa_candidates(&[segment], &[translation]);

        assert!(
            candidates.is_empty(),
            "matching inline markers alone should not make a segment suspicious"
        );
    }

    #[test]
    fn suspicious_qa_includes_marker_id_mismatch() {
        let segment = marked_segment("seg_marker", 0, "<m1>Hello</m1>");
        let translation = translation_for(
            &segment,
            "<m2>Ciao</m2>",
            "stored",
            SegmentStatus::Succeeded,
        );

        let candidates = suspicious_qa_candidates(&[segment], &[translation]);

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].segment_id.0, "seg_marker");
    }

    #[test]
    fn suspicious_qa_includes_marker_shape_mismatch() {
        let segment = marked_segment("seg_marker", 0, "<m1>Hello</m1>");
        let translation = translation_for(&segment, "<m1/>", "stored", SegmentStatus::Succeeded);

        let candidates = suspicious_qa_candidates(&[segment], &[translation]);

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].segment_id.0, "seg_marker");
    }

    #[test]
    fn suspicious_qa_includes_malformed_marker() {
        let segment = marked_segment("seg_marker", 0, "<m1>Hello</m1>");
        let translation = translation_for(&segment, "<m1>Ciao", "stored", SegmentStatus::Succeeded);

        let candidates = suspicious_qa_candidates(&[segment], &[translation]);

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].segment_id.0, "seg_marker");
    }

    fn marked_segment(id: &str, ordinal: usize, text: &str) -> Segment {
        let mut segment = segment(id, ordinal);
        segment.source.text = text.to_string();
        segment.source.blocks[0].text = text.to_string();
        segment.constraints.preserve_markers = bookforge_core::marker::marker_ids_in_text(text);
        segment
    }

    fn translation_for(
        segment: &Segment,
        text: &str,
        template: &str,
        status: SegmentStatus,
    ) -> SegmentTranslation {
        SegmentTranslation {
            segment_id: segment.id.clone(),
            ordinal: segment.ordinal,
            block_ids: segment.block_ids.clone(),
            blocks: vec![BlockTranslation {
                block_id: segment.source.blocks[0].block_id.clone(),
                text: text.to_string(),
            }],
            checksum: segment.checksum.clone(),
            status,
            template: template.to_string(),
            error: None,
            input_tokens: Some(1),
            input_cached_tokens: Some(0),
            output_tokens: Some(1),
            tokens_estimated: false,
        }
    }

    fn segment(id: &str, ordinal: usize) -> Segment {
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

    fn temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "bookforge-cli-test-{}-{nanos}-{name}",
            std::process::id()
        ))
    }

    #[test]
    fn provider_config_sets_provider_max_attempts() {
        use bookforge_core::RetryAfterPolicy;
        let cfg = provider_config(
            "openrouter",
            None,
            None,
            None,
            120,
            2,
            false,
            RetryAfterPolicy::JitteredExponential,
            30,
            32,
            bookforge_core::JsonMode::Auto,
        )
        .expect("provider_config should build");
        assert_eq!(cfg.provider_max_attempts, 2);

        let cfg = provider_config(
            "openrouter",
            None,
            None,
            None,
            120,
            0,
            false,
            RetryAfterPolicy::JitteredExponential,
            30,
            32,
            bookforge_core::JsonMode::Auto,
        )
        .expect("provider_config should build");
        assert_eq!(cfg.provider_max_attempts, 1);
    }

    #[test]
    fn provider_config_uses_resolved_json_mode() {
        let cfg = provider_config(
            "openrouter",
            None,
            None,
            None,
            120,
            1,
            true,
            bookforge_core::RetryAfterPolicy::JitteredExponential,
            15,
            64,
            bookforge_core::JsonMode::PromptOnly,
        )
        .expect("provider_config should build");

        assert_eq!(cfg.json_mode, bookforge_core::JsonMode::PromptOnly);
    }

    #[test]
    fn retry_amplification_warning_emitted_for_high_attempt_product() {
        let mut settings = TranslationProfile::Safe.resolve();
        settings.scheduler.max_attempts = 3;
        settings.provider.provider_max_attempts = 2;
        settings.provider.validation_max_attempts = 1;

        let warning = retry_amplification_warning(&settings).expect("warning expected");
        assert!(warning.contains("scheduler attempts 3 x provider attempts 2"));
        assert!(warning.contains("up to 6 calls"));
    }

    fn translate_args_with_preset(
        provider_preset: Option<bookforge_core::ProviderPreset>,
    ) -> TranslateArgs {
        TranslateArgs {
            input: temp_path("input.epub"),
            language: LanguageArgs {
                source: Some("English".to_string()),
                target: "Italian".to_string(),
            },
            provider: CliProviderArgs {
                provider: "deepseek".to_string(),
                model: None,
                base_url: None,
                api_key_env: None,
                timeout_seconds: None,
            },
            profile: TranslationProfile::V1Fast,
            max_segment_tokens: None,
            context_tokens: None,
            batch_target_tokens: None,
            batch_max_items: None,
            compact_prompts: None,
            retry_failed_only: None,
            adaptive_concurrency: None,
            turbo_text_only: false,
            concurrency: None,
            max_attempts: None,
            provider_max_attempts: None,
            validation_max_attempts: None,
            out: None,
            validate_output: false,
            strict_epubcheck: false,
            book_id: None,
            series_id: None,
            glossary: Vec::new(),
            glossary_budget_tokens: 800,
            glossary_format: GlossaryFormat::Json,
            prompt_extra: None,
            context_window: 0,
            context_budget_tokens: 1200,
            context_scope: bookforge_core::config::ContextScope::Chapter,
            context_strict: false,
            style: Vec::new(),
            entities: Vec::new(),
            qa: QaMode::Off,
            qa_concurrency: 8,
            qa_batch_target_tokens: None,
            qa_model: None,
            qa_provider: None,
            qa_base_url: None,
            qa_api_key_env: None,
            double_check: DoubleCheckMode::Off,
            double_check_model: None,
            double_check_provider: None,
            double_check_base_url: None,
            double_check_api_key_env: None,
            double_check_concurrency: 4,
            double_check_batch_target_tokens: None,
            auto_correct: false,
            correction_rounds: 1,
            fallback_provider: None,
            fallback_model: None,
            fallback_base_url: None,
            fallback_api_key_env: None,
            fallback_only: FallbackScope::Failed,
            no_thinking: false,
            model_context_tokens: None,
            max_output_tokens: None,
            batch_max_output_tokens: None,
            json_mode: bookforge_core::JsonMode::Auto,
            ui: crate::progress::UiMode::Quiet,
            progress_jsonl: None,
            provider_preset,
        }
    }

    #[test]
    fn explicit_cli_concurrency_overrides_provider_preset() {
        let mut args =
            translate_args_with_preset(Some(bookforge_core::ProviderPreset::OpenRouterPaidFast));
        args.concurrency = Some(8);
        let settings = resolve_settings(&args);
        assert_eq!(settings.scheduler.concurrency, 8);
    }

    #[test]
    fn explicit_cli_provider_max_attempts_overrides_provider_preset() {
        let mut args =
            translate_args_with_preset(Some(bookforge_core::ProviderPreset::OpenRouterPaidFast));
        args.provider_max_attempts = Some(3);
        let settings = resolve_settings(&args);
        assert_eq!(settings.provider.provider_max_attempts, 3);
    }

    #[test]
    fn provider_preset_runtime_is_reflected_in_resolved_settings() {
        let args = translate_args_with_preset(Some(bookforge_core::ProviderPreset::OpenRouterFree));
        let settings = resolve_settings(&args);
        assert_eq!(settings.scheduler.concurrency, 2);
        assert_eq!(
            settings.provider.retry_after_policy,
            bookforge_core::RetryAfterPolicy::RespectHeader
        );
        assert_eq!(settings.provider.max_idle_per_host, 8);
    }
}
