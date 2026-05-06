use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
};

use anyhow::Result;
use bookforge_core::{
    ProgressEvent, ProgressSink, ResolvedRunSettings, RunConfigSnapshot,
    progress::now_ms,
    segment::{BlockTranslation, Segment, SegmentStatus, build_segments, compute_cache_namespace},
};
use bookforge_epub::{read_epub, rebuild_epub};
use bookforge_llm::{
    MockProvider, OpenAiCompatibleConfig, OpenAiCompatibleProvider, QaSegmentReview,
    SegmentTranslation, TranslationRunConfig,
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
    let Some(mut snapshot) = store.load_job_config_snapshot(&args.job_id)? else {
        anyhow::bail!(
            "job '{}' does not have a run configuration snapshot; it cannot be resumed deterministically",
            args.job_id
        );
    };

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
    let input = snapshot.input_path.clone();
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

    println!("Job: {}", job.id);
    println!("Input: {}", input.display());
    println!("Output: {}", output.display());
    println!("Provider: {}", job.provider);
    println!("Pending: {}", pending_ids.len());

    let pending_segments = select_pending_segments(&segments, &pending_ids)?;
    let prompt_version = snapshot.prompt_version.as_str();
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
    };

    let cache_namespace = compute_cache_namespace(
        settings.segmentation.max_segment_tokens,
        settings.segmentation.context_tokens,
        run_config.profile.namespace_str(),
        settings.batch.enabled,
        prompt_version,
    );
    if cache_namespace != snapshot.cache_namespace {
        anyhow::bail!(
            "resume cache namespace mismatch for job '{}': snapshot={}, recomputed={}",
            job.id,
            snapshot.cache_namespace,
            cache_namespace
        );
    }
    let mut cached_translations = apply_cached_translations(
        &pending_segments,
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

    Ok(())
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
    }
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
                output_tokens: None,
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
