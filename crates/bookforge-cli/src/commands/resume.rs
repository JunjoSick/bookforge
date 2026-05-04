use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use anyhow::Result;
use bookforge_core::{
    config::SegmentationConfig,
    config::TranslationProfile,
    scheduler::SchedulerConfig,
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
    default_output_path,
    report::{ReportInput, write_report},
};

use super::translate::{
    CacheContext, CheckpointContext, apply_cached_translations, mock_mode,
    pending_segments_for_job, qa_reviews_for_mode, translate_and_checkpoint,
};

#[derive(Debug, Args)]
pub struct ResumeArgs {
    pub job_id: String,

    #[arg(long, default_value_t = 4)]
    pub concurrency: usize,

    #[arg(long, default_value_t = 3)]
    pub max_attempts: usize,

    #[arg(long, value_enum, default_value_t = QaMode::Off)]
    pub qa: QaMode,

    #[arg(long, default_value_t = 120)]
    pub timeout_seconds: u64,
}

pub async fn run(args: ResumeArgs) -> Result<()> {
    let store = JobStore::open_default()?;
    let Some(job) = store.get_job(&args.job_id)? else {
        anyhow::bail!("job '{}' was not found", args.job_id);
    };

    let input = job_input_path(&job)?;
    let output = job_output_path(&job);
    let book = read_epub(&input)?;
    let segments = build_segments(&book, &SegmentationConfig::default())?;
    let pending_ids = store.pending_segment_ids(&job.id)?;

    println!("Job: {}", job.id);
    println!("Input: {}", input.display());
    println!("Output: {}", output.display());
    println!("Provider: {}", job.provider);
    println!("Pending: {}", pending_ids.len());

    let pending_segments = pending_segments(&segments, &pending_ids)?;
    let prompt_version = "v1";
    let run_config = TranslationRunConfig {
        source_language: job.source_lang.clone(),
        target_language: job.target_lang.clone(),
        provider: job.provider.clone(),
        model: job.model.clone(),
        prompt_version: prompt_version.to_string(),
        temperature: 0.2,
        scheduler: SchedulerConfig {
            concurrency: args.concurrency,
            max_attempts: args.max_attempts,
        },
        profile: TranslationProfile::Balanced,
    };

    // Resume currently rebuilds segments from defaults; namespace must
    // match what insert_segments stored on the original run.
    let segmentation = SegmentationConfig::default();
    let cache_namespace = compute_cache_namespace(
        segmentation.max_segment_tokens,
        segmentation.context_tokens,
        &format!("{:?}", run_config.profile),
        false,
        prompt_version,
    );
    let mut cached_translations = apply_cached_translations(
        &pending_segments,
        CacheContext {
            store: &store,
            job_id: &job.id,
            prompt_version,
            provider: &job.provider,
            model: &job.model,
            source_lang: job.source_lang.as_deref(),
            target_lang: &job.target_lang,
            cache_namespace: &cache_namespace,
        },
    )?;
    let pending_segments = pending_segments_for_job(&store, &job.id, &segments)?;

    let fresh_translations = if pending_segments.is_empty() {
        Vec::new()
    } else {
        let writer = CheckpointWriter::spawn(store.path().to_path_buf());
        let tx = writer.sender();
        let result = match job.provider.as_str() {
            "mock" => {
                let provider = MockProvider::new(mock_mode(&job.model), &job.target_lang);
                translate_and_checkpoint(
                    provider.clone(),
                    &pending_segments,
                    &run_config,
                    CheckpointContext {
                        store: &store,
                        job_id: &job.id,
                        provider: &job.provider,
                        model: &job.model,
                        prompt_version,
                        tx: &tx,
                    },
                )
                .await
            }
            "deepseek" | "openrouter" | "openai-compatible" => {
                let provider_config = openai_compatible_config(&job, args.timeout_seconds, 6)?;
                let provider = OpenAiCompatibleProvider::new(provider_config)?;
                translate_and_checkpoint(
                    provider.clone(),
                    &pending_segments,
                    &run_config,
                    CheckpointContext {
                        store: &store,
                        job_id: &job.id,
                        provider: &job.provider,
                        model: &job.model,
                        prompt_version,
                        tx: &tx,
                    },
                )
                .await
            }
            provider => anyhow::bail!("cannot resume unsupported provider '{provider}'"),
        };
        drop(tx);
        writer.shutdown().await?;
        result?
    };

    cached_translations.extend(fresh_translations);
    cached_translations.sort_by_key(|translation| translation.ordinal);
    mark_job_from_summary(&store, &job.id)?;

    let stored_blocks = store.load_block_translations(&job.id)?;
    let translations = rebuild_segment_translations(&segments, &stored_blocks);
    let qa_reviews = qa_after_resume(
        &job,
        &segments,
        &translations,
        &run_config,
        args.qa,
        args.timeout_seconds,
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
    let segment_records = store.segment_records(&job.id)?;
    let report = write_report(ReportInput {
        job: &job,
        summary: &summary,
        segments: &segments,
        segment_records: &segment_records,
        translations: &translations,
        qa_reviews: &qa_reviews,
        output: &output,
    })?;

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

fn job_input_path(job: &JobRecord) -> Result<PathBuf> {
    if job.input_path.as_os_str().is_empty() {
        anyhow::bail!(
            "job '{}' does not have an input path; it was created before resume metadata was persisted",
            job.id
        );
    }
    Ok(job.input_path.clone())
}

fn job_output_path(job: &JobRecord) -> PathBuf {
    if job.output_path.as_os_str().is_empty() {
        default_output_path(&job.input_path, &job.target_lang)
    } else {
        job.output_path.clone()
    }
}

fn pending_segments(segments: &[Segment], pending_ids: &[String]) -> Result<Vec<Segment>> {
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
    timeout_seconds: u64,
    provider_max_attempts: usize,
) -> Result<OpenAiCompatibleConfig> {
    let provider_max_attempts = provider_max_attempts.max(1);
    if job.provider == "deepseek" {
        let mut config = OpenAiCompatibleConfig::deepseek(Some(job.model.clone()));
        if let Some(base_url) = &job.base_url {
            config.base_url = base_url.clone();
        }
        if let Some(api_key_env) = &job.api_key_env {
            config.api_key_env = api_key_env.clone();
        }
        config.timeout_seconds = timeout_seconds;
        config.provider_max_attempts = provider_max_attempts;
        return Ok(config);
    }

    if job.provider == "openrouter" {
        return Ok(OpenAiCompatibleConfig {
            base_url: job
                .base_url
                .clone()
                .unwrap_or_else(|| "https://openrouter.ai/api/v1".to_string()),
            api_key_env: job
                .api_key_env
                .clone()
                .unwrap_or_else(|| "OPENROUTER_API_KEY".to_string()),
            model: job.model.clone(),
            timeout_seconds,
            provider_max_attempts,
            thinking_disabled: false,
        });
    }

    Ok(OpenAiCompatibleConfig {
        base_url: job.base_url.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "job '{}' does not have a stored base URL for openai-compatible resume",
                job.id
            )
        })?,
        api_key_env: job
            .api_key_env
            .clone()
            .unwrap_or_else(|| "OPENAI_API_KEY".to_string()),
        model: job.model.clone(),
        timeout_seconds,
        provider_max_attempts,
        thinking_disabled: false,
    })
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
    qa_mode: QaMode,
    timeout_seconds: u64,
) -> Result<Vec<QaSegmentReview>> {
    match job.provider.as_str() {
        "mock" => {
            let provider = MockProvider::new(mock_mode(&job.model), &job.target_lang);
            Ok(qa_reviews_for_mode(provider, segments, translations, config, qa_mode).await)
        }
        "deepseek" | "openrouter" | "openai-compatible" => {
            let provider_config = openai_compatible_config(job, timeout_seconds, 6)?;
            let provider = OpenAiCompatibleProvider::new(provider_config)?;
            Ok(qa_reviews_for_mode(provider, segments, translations, config, qa_mode).await)
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
) -> Vec<SegmentTranslation> {
    let mut by_segment_block = HashMap::<(String, String), String>::new();
    for translation in stored {
        by_segment_block.insert(
            (translation.segment_id.clone(), translation.block_id.clone()),
            translation.text.clone(),
        );
    }

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
            translations.push(SegmentTranslation {
                segment_id: segment.id.clone(),
                ordinal: segment.ordinal,
                block_ids: segment.block_ids.clone(),
                blocks,
                checksum: segment.checksum.clone(),
                status: SegmentStatus::Succeeded,
                template: "stored".to_string(),
                error: None,
                input_tokens: None,
                output_tokens: None,
            });
        }
    }

    translations
}
