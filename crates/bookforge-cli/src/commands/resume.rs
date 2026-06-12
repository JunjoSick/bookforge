use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
};

use anyhow::Result;
use bookforge_core::{
    ProgressEvent, ProgressSink, ResolvedRunSettings, RunConfigSnapshot, merge_scope_terms,
    progress::now_ms,
    segment::{
        BlockTranslation, Segment, SegmentStatus, build_segments, compute_cache_namespace,
        compute_cache_namespace_v1,
    },
    select_glossary_for_segments,
};
use bookforge_epub::{read_epub, rebuild_epub};
use bookforge_llm::{
    ContextRegistry, ContextRunConfig, EntityRunConfig, GlossaryRunConfig, MockProvider,
    OpenAiCompatibleConfig, OpenAiCompatibleProvider, QaSegmentReview, SegmentTranslation,
    StyleRunConfig, TranslationRunConfig,
};
use bookforge_store::{JobRecord, JobStore, StoredBlockTranslation};
use clap::Args;

use crate::{
    QaMode,
    checkpoint::CheckpointWriter,
    cost::estimate_cost_usd,
    performance::performance_summary_from_events,
    report::{ReportInput, write_report},
};

use super::translate::{
    CacheContext, CheckpointContext, apply_cached_translations, mock_mode, qa_reviews_for_mode,
    translate_and_checkpoint, translate_and_checkpoint_batch,
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

    #[arg(long, value_enum, default_value_t = QaMode::Off)]
    pub qa: QaMode,

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
}

pub async fn run(args: ResumeArgs) -> Result<()> {
    let store = JobStore::open_default()?;
    let Some(job) = store.get_job(&args.job_id)? else {
        anyhow::bail!("job '{}' was not found", args.job_id);
    };
    let mut snapshot = load_resume_snapshot(&store, &args.job_id)?;

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

    let run_result = run_inner(args, store, job, &mut snapshot, progress).await;
    finalize_reporter(run_result, reporter).await
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
    let glossary = if legacy_cache_namespace {
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
            },
            fingerprint,
            active_terms,
        }
    };
    let context_cfg = snapshot_context_run_config(snapshot);
    let context_registry: Option<Arc<ContextRegistry>> = if context_cfg.enabled() {
        let registry = Arc::new(ContextRegistry::new(&segments));
        rehydrate_context_registry_from_store(&registry, &segments, &store, &job.id)?;
        Some(registry)
    } else {
        None
    };
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
    let mut cached_translations = apply_cached_translations(
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

    let fresh_translations = if pending_segments.is_empty() {
        Vec::new()
    } else {
        let writer = CheckpointWriter::spawn(store.path().to_path_buf(), progress.clone());
        let sender = writer.sender();
        let result = match job.provider.as_str() {
            "mock" => {
                let provider =
                    MockProvider::new(mock_mode(&snapshot.model), &snapshot.target_language);
                if settings.batch.enabled {
                    let batch_run_config = batch_run_config(&run_config, &settings);
                    translate_and_checkpoint_batch(
                        provider.clone(),
                        &pending_segments,
                        &batch_run_config,
                        &settings,
                        CheckpointContext {
                            store: &store,
                            job_id: &job.id,
                            provider: &snapshot.provider,
                            model: &snapshot.model,
                            prompt_version,
                            sender: &sender,
                        },
                        progress.clone(),
                    )
                    .await
                } else {
                    translate_and_checkpoint(
                        provider.clone(),
                        &pending_segments,
                        &run_config,
                        CheckpointContext {
                            store: &store,
                            job_id: &job.id,
                            provider: &snapshot.provider,
                            model: &snapshot.model,
                            prompt_version,
                            sender: &sender,
                        },
                    )
                    .await
                }
            }
            "deepseek" | "openrouter" | "openai-compatible" => {
                let provider_config = openai_compatible_config(&job, snapshot, &settings)?;
                let provider = OpenAiCompatibleProvider::new(provider_config)?;
                if settings.batch.enabled {
                    let batch_run_config = batch_run_config(&run_config, &settings);
                    translate_and_checkpoint_batch(
                        provider.clone(),
                        &pending_segments,
                        &batch_run_config,
                        &settings,
                        CheckpointContext {
                            store: &store,
                            job_id: &job.id,
                            provider: &snapshot.provider,
                            model: &snapshot.model,
                            prompt_version,
                            sender: &sender,
                        },
                        progress.clone(),
                    )
                    .await
                } else {
                    translate_and_checkpoint(
                        provider.clone(),
                        &pending_segments,
                        &run_config,
                        CheckpointContext {
                            store: &store,
                            job_id: &job.id,
                            provider: &snapshot.provider,
                            model: &snapshot.model,
                            prompt_version,
                            sender: &sender,
                        },
                    )
                    .await
                }
            }
            provider => anyhow::bail!("cannot resume unsupported provider '{provider}'"),
        };
        drop(sender);
        writer.shutdown().await?;
        result?
    };

    cached_translations.extend(fresh_translations);
    cached_translations.sort_by_key(|translation| translation.ordinal);
    mark_job_from_summary(&store, &job.id)?;

    let stored_blocks = store.load_block_translations(&job.id)?;
    let segment_records = store.segment_records(&job.id)?;
    let translations = rebuild_segment_translations(&segments, &stored_blocks, &segment_records);
    let qa_reviews = qa_after_resume(
        &job,
        &segments,
        &translations,
        &run_config,
        snapshot,
        &settings,
        args.qa,
    )
    .await?;
    let block_translations =
        rebuild_block_translations(&segments, &stored_blocks, &cached_translations);
    rebuild_epub(&book, &block_translations, &output)?;

    let job = store
        .get_job(&job.id)?
        .ok_or_else(|| anyhow::anyhow!("job '{}' was not found after resume", job.id))?;
    let summary = store
        .summary(&job.id)?
        .ok_or_else(|| anyhow::anyhow!("job '{}' was not found after resume", job.id))?;
    let report = write_report(ReportInput {
        job: &job,
        summary: &summary,
        segments: &segments,
        segment_records: &segment_records,
        translations: &translations,
        qa_reviews: &qa_reviews,
        performance: snapshot
            .events_path
            .as_ref()
            .and_then(|path| performance_summary_from_events(path).ok().flatten()),
        output: &output,
    })?;
    store.update_job_report_paths(&job.id, &report.json, &report.markdown)?;
    snapshot.report_json_path = Some(report.json.clone());
    snapshot.report_markdown_path = Some(report.markdown.clone());
    store.update_job_config_snapshot(&job.id, snapshot)?;
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
        if let Some(cost) = estimate_cost_usd(
            &job.provider,
            &job.model,
            summary.input_tokens,
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
        Some(crate::progress::UiMode::Json | crate::progress::UiMode::Quiet)
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

fn batch_run_config(
    run_config: &TranslationRunConfig,
    settings: &ResolvedRunSettings,
) -> TranslationRunConfig {
    TranslationRunConfig {
        source_language: run_config.source_language.clone(),
        target_language: run_config.target_language.clone(),
        provider: run_config.provider.clone(),
        model: run_config.model.clone(),
        prompt_version: run_config.prompt_version.clone(),
        temperature: run_config.temperature,
        scheduler: bookforge_core::SchedulerConfig {
            concurrency: run_config.scheduler.concurrency,
            max_attempts: settings.provider.provider_max_attempts,
        },
        profile: settings.profile,
        model_context_tokens: settings.provider.model_context_tokens,
        max_output_tokens: settings.provider.max_output_tokens,
        batch_max_output_tokens: settings.provider.batch_max_output_tokens,
        compact_prompts: settings.compact_prompts,
        glossary: run_config.glossary.clone(),
        context: run_config.context,
        context_registry: run_config.context_registry.clone(),
        style: run_config.style.clone(),
        entities: run_config.entities.clone(),
    }
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

fn mark_job_from_summary(store: &JobStore, job_id: &str) -> Result<()> {
    let Some(summary) = store.summary(job_id)? else {
        anyhow::bail!("job '{job_id}' was not found");
    };

    if summary.failed > 0 || summary.needs_review > 0 || summary.retry_pending > 0 {
        store.mark_job_needs_review(job_id)?;
    } else {
        store.mark_job_complete(job_id)?;
    }
    Ok(())
}

async fn qa_after_resume(
    job: &JobRecord,
    segments: &[Segment],
    translations: &[SegmentTranslation],
    config: &TranslationRunConfig,
    snapshot: &RunConfigSnapshot,
    settings: &ResolvedRunSettings,
    qa_mode: QaMode,
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

    match provider_name {
        "mock" => {
            let provider = MockProvider::new(mock_mode(model), &job.target_lang);
            Ok(
                qa_reviews_for_mode(provider, segments, translations, config, qa_config, qa_mode)
                    .await,
            )
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
            let provider = OpenAiCompatibleProvider::new(provider_config)?;
            Ok(
                qa_reviews_for_mode(provider, segments, translations, config, qa_config, qa_mode)
                    .await,
            )
        }
        _ => Ok(Vec::new()),
    }
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
    use std::sync::Mutex;

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
    async fn resume_uses_snapshot_segmentation_settings() {
        let mut settings = TranslationProfile::V1Fast.resolve();
        settings.batch.enabled = false;
        settings.segmentation.max_segment_tokens = 1377;
        settings.segmentation.context_tokens = 77;
        let Some(mut fixture) = resume_fixture(settings, 1) else {
            return;
        };

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
        let Some(mut fixture) = resume_fixture(settings, 1) else {
            return;
        };

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
        let Some(mut fixture) = resume_fixture(settings.clone(), 1) else {
            return;
        };

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
    async fn resume_missing_config_snapshot_fails_clearly() {
        let tempdir = tempfile::tempdir().expect("tempdir should be created");
        let Some(input) = fixture_input_or_skip() else {
            return;
        };
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
        let Some(mut fixture) = resume_fixture(settings, 2) else {
            return;
        };
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
        let Some(mut fixture) = resume_fixture(settings, 1) else {
            return;
        };
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
        let Some(mut fixture) = resume_fixture(settings, 2) else {
            return;
        };
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
        let Some(mut fixture) = resume_fixture(settings, 1) else {
            return;
        };
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
        let Some(mut fixture) = resume_fixture(settings.clone(), 1) else {
            return;
        };
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

    fn resume_fixture(
        settings: ResolvedRunSettings,
        segment_count: usize,
    ) -> Option<ResumeFixture> {
        let tempdir = tempfile::tempdir().expect("tempdir should be created");
        let input = fixture_input_or_skip()?;
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
            settings: bookforge_core::ResolvedRunSettingsSnapshot::from_settings(&settings),
        };
        store
            .update_job_config_snapshot(&job.id, &snapshot)
            .expect("snapshot should persist");

        Some(ResumeFixture {
            _tempdir: tempdir,
            store,
            job,
            snapshot,
            segments,
        })
    }

    async fn run_fixture(fixture: &mut ResumeFixture) -> Result<Vec<ProgressEvent>> {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::new(RecordingSink {
            events: events.clone(),
        });
        let run_store = JobStore::open(fixture.store.path()).expect("store should reopen");
        let args = ResumeArgs {
            job_id: fixture.job.id.clone(),
            concurrency: None,
            max_attempts: None,
            provider_max_attempts: None,
            validation_max_attempts: None,
            qa: QaMode::Off,
            timeout_seconds: None,
            max_output_tokens: None,
            ui: None,
            progress_jsonl: None,
            output: None,
            no_thinking: false,
        };

        run_inner(
            args,
            run_store,
            fixture.job.clone(),
            &mut fixture.snapshot,
            sink,
        )
        .await?;

        Ok(events.lock().expect("events lock").clone())
    }

    fn runtime_config_event(events: &[ProgressEvent]) -> RuntimeConfigEvent<'_> {
        events
            .iter()
            .find_map(|event| {
                if let ProgressEvent::RuntimeConfigResolved {
                    profile,
                    provider_max_attempts,
                    compact_prompts,
                    json_mode,
                    ..
                } = event
                {
                    Some(RuntimeConfigEvent {
                        profile,
                        provider_max_attempts: *provider_max_attempts,
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

    // Returns the path to test/test.epub if present, otherwise prints a skip
    // line and returns None. The fixture is gitignored (private to the
    // maintainer), so CI and contributor checkouts must skip these tests
    // rather than fail. See CLAUDE.md and CONTRIBUTING.md.
    fn fixture_input_or_skip() -> Option<PathBuf> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .expect("cli crate should be under crates/bookforge-cli")
            .join("test/test.epub");
        if !path.exists() {
            let name = std::thread::current().name().unwrap_or("?").to_string();
            eprintln!("[skip] {name}: requires test/test.epub fixture");
            return None;
        }
        Some(path)
    }

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
