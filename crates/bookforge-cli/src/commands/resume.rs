use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use anyhow::Result;
use bookforge_core::{
    config::SegmentationConfig,
    scheduler::SchedulerConfig,
    segment::{BlockTranslation, Segment, build_segments},
};
use bookforge_epub::{read_epub, rebuild_epub};
use bookforge_llm::{
    MockProvider, OpenAiCompatibleConfig, OpenAiCompatibleProvider, SegmentTranslation,
    TranslationRunConfig, qa_segments,
};
use bookforge_store::{JobRecord, JobStore, StoredBlockTranslation};
use clap::Args;

use crate::{
    cost::estimate_cost_usd,
    default_output_path,
    report::{ReportInput, write_report},
};

use super::translate::{mock_mode, save_translation_result, translate_with_scheduler_guard};

#[derive(Debug, Args)]
pub struct ResumeArgs {
    pub job_id: String,

    #[arg(long, default_value_t = 4)]
    pub concurrency: usize,
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
            max_retries: 3,
        },
    };

    let (translations, qa_reviews) = if pending_segments.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        match job.provider.as_str() {
            "mock" => {
                let provider = MockProvider::new(mock_mode(&job.model), &job.target_lang);
                let translations = translate_with_scheduler_guard(
                    provider.clone(),
                    &store,
                    &job.id,
                    &pending_segments,
                    &run_config,
                )
                .await?;
                let qa_reviews =
                    qa_segments(provider, &pending_segments, &translations, &run_config).await;
                (translations, qa_reviews)
            }
            "deepseek" | "openai-compatible" => {
                let provider_config = openai_compatible_config(&job)?;
                let provider = OpenAiCompatibleProvider::new(provider_config)?;
                let translations = translate_with_scheduler_guard(
                    provider.clone(),
                    &store,
                    &job.id,
                    &pending_segments,
                    &run_config,
                )
                .await?;
                let qa_reviews =
                    qa_segments(provider, &pending_segments, &translations, &run_config).await;
                (translations, qa_reviews)
            }
            provider => anyhow::bail!("cannot resume unsupported provider '{provider}'"),
        }
    };

    for translation in &translations {
        save_translation_result(
            &store,
            &job.id,
            translation,
            &job.provider,
            &job.model,
            prompt_version,
        )?;
    }
    mark_job_from_summary(&store, &job.id)?;

    let stored_blocks = store.load_block_translations(&job.id)?;
    let block_translations = rebuild_block_translations(&segments, &stored_blocks, &translations);
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

fn openai_compatible_config(job: &JobRecord) -> Result<OpenAiCompatibleConfig> {
    if job.provider == "deepseek" {
        let mut config = OpenAiCompatibleConfig::deepseek(Some(job.model.clone()));
        if let Some(base_url) = &job.base_url {
            config.base_url = base_url.clone();
        }
        if let Some(api_key_env) = &job.api_key_env {
            config.api_key_env = api_key_env.clone();
        }
        return Ok(config);
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
        timeout_seconds: 120,
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
