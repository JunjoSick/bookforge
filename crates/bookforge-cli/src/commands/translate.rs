use anyhow::Result;
use bookforge_core::{
    config::{SegmentationConfig, TranslationConfig},
    scheduler::SchedulerConfig,
    segment::{BlockTranslation, Segment, SegmentStatus, build_segments},
};
use bookforge_epub::{read_epub, rebuild_epub};
#[cfg(test)]
use bookforge_llm::translate_segments;
use bookforge_llm::{
    LlmError, LlmProvider, MockMode, MockProvider, OpenAiCompatibleConfig,
    OpenAiCompatibleProvider, QaSegmentReview, SegmentTranslation, TranslationRunConfig,
    qa_segments, translate_segments_with_callback,
};
use bookforge_store::{
    CreateJob, JobRecord, JobStore, SaveCachedTranslation, SaveNeedsReview, SaveTranslation,
};
use clap::Args;
use std::path::PathBuf;

use crate::{
    LanguageArgs, ProviderArgs, QaMode,
    cost::estimate_cost_usd,
    default_output_path,
    report::{ReportInput, write_report},
};

#[derive(Debug, Args)]
pub struct TranslateArgs {
    pub input: PathBuf,

    #[command(flatten)]
    pub language: LanguageArgs,

    #[command(flatten)]
    pub provider: ProviderArgs,

    #[arg(long, default_value_t = 4)]
    pub concurrency: usize,

    #[arg(long)]
    pub out: Option<PathBuf>,

    #[arg(long, value_enum, default_value_t = QaMode::Off)]
    pub qa: QaMode,
}

pub async fn run(args: TranslateArgs) -> Result<()> {
    let output = args
        .out
        .unwrap_or_else(|| default_output_path(&args.input, &args.language.target));
    let config = TranslationConfig {
        source_language: args.language.source,
        target_language: args.language.target,
        provider: args.provider.provider.clone(),
        model: args.provider.model.clone(),
        concurrency: args.concurrency,
        output,
    };

    println!("Input: {}", args.input.display());
    println!("Output: {}", config.output.display());
    println!("Target: {}", config.target_language);
    println!("Provider: {}", config.provider);
    println!("Concurrency: {}", config.concurrency);

    match config.provider.as_str() {
        "mock" => run_mock_translation(&args.input, &config, args.qa).await?,
        "deepseek" | "openrouter" | "openai-compatible" => {
            run_openai_compatible_translation(&args.input, &config, &args.provider, args.qa).await?
        }
        _ => {
            println!(
                "Translation provider '{}' is not implemented yet.",
                config.provider
            );
        }
    }

    Ok(())
}

async fn run_mock_translation(
    input: &PathBuf,
    config: &TranslationConfig,
    qa_mode: QaMode,
) -> Result<()> {
    let book = read_epub(input)?;
    let segments = build_segments(&book, &SegmentationConfig::default())?;
    let model = config
        .model
        .clone()
        .unwrap_or_else(|| "mock-prefix-target".to_string());
    let prompt_version = "v1";
    let store = JobStore::open_default()?;
    let job = store.create_job(CreateJob {
        input,
        output: &config.output,
        source_lang: config.source_language.as_deref(),
        target_lang: &config.target_language,
        provider: "mock",
        model: &model,
        base_url: None,
        api_key_env: None,
    })?;
    println!("Job: {}", job.id);
    store.insert_segments(&job.id, &segments, prompt_version, "mock", &model)?;
    let run_config = TranslationRunConfig {
        source_language: config.source_language.clone(),
        target_language: config.target_language.clone(),
        provider: "mock".to_string(),
        model: model.clone(),
        prompt_version: prompt_version.to_string(),
        temperature: 0.2,
        scheduler: SchedulerConfig {
            concurrency: config.concurrency,
            max_retries: 3,
        },
    };
    let provider = MockProvider::new(mock_mode(&model), &config.target_language);
    let mut translations = apply_cached_translations(
        &segments,
        CacheContext {
            store: &store,
            job_id: &job.id,
            prompt_version,
            provider: "mock",
            model: &model,
            source_lang: config.source_language.as_deref(),
            target_lang: &config.target_language,
        },
    )?;
    let pending_segments = pending_segments_for_job(&store, &job.id, &segments)?;
    let fresh_translations = translate_and_checkpoint(
        provider.clone(),
        &pending_segments,
        &run_config,
        CheckpointContext {
            store: &store,
            job_id: &job.id,
            provider: "mock",
            model: &model,
            prompt_version,
        },
    )
    .await?;
    translations.extend(fresh_translations);
    translations.sort_by_key(|translation| translation.ordinal);
    let qa_reviews =
        qa_reviews_for_mode(provider, &segments, &translations, &run_config, qa_mode).await;
    mark_job_finished(&store, &job.id, &translations)?;
    print_summary_rebuild_and_report(
        &store,
        &job,
        &book,
        &segments,
        &translations,
        &qa_reviews,
        config,
    )?;

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
    provider_args: &crate::ProviderArgs,
    qa_mode: QaMode,
) -> Result<()> {
    let provider_config = if config.provider == "deepseek" {
        let mut config = OpenAiCompatibleConfig::deepseek(config.model.clone());
        config.timeout_seconds = provider_args.timeout_seconds;
        config
    } else if config.provider == "openrouter" {
        OpenAiCompatibleConfig {
            base_url: provider_args
                .base_url
                .clone()
                .unwrap_or_else(|| "https://openrouter.ai/api/v1".to_string()),
            api_key_env: provider_args
                .api_key_env
                .clone()
                .unwrap_or_else(|| "OPENROUTER_API_KEY".to_string()),
            model: config
                .model
                .clone()
                .unwrap_or_else(|| "openrouter/auto".to_string()),
            timeout_seconds: provider_args.timeout_seconds,
        }
    } else {
        OpenAiCompatibleConfig {
            base_url: provider_args.base_url.clone().ok_or_else(|| {
                anyhow::anyhow!("--base-url is required for --provider openai-compatible")
            })?,
            api_key_env: provider_args
                .api_key_env
                .clone()
                .unwrap_or_else(|| "OPENAI_API_KEY".to_string()),
            model: config
                .model
                .clone()
                .ok_or_else(|| anyhow::anyhow!("--model is required for openai-compatible"))?,
            timeout_seconds: provider_args.timeout_seconds,
        }
    };
    let provider = OpenAiCompatibleProvider::new(provider_config.clone())?;
    let model = provider.model().to_string();
    let book = read_epub(input)?;
    let segments = build_segments(&book, &SegmentationConfig::default())?;
    let prompt_version = "v1";
    let store = JobStore::open_default()?;
    let job = store.create_job(CreateJob {
        input,
        output: &config.output,
        source_lang: config.source_language.as_deref(),
        target_lang: &config.target_language,
        provider: &config.provider,
        model: &model,
        base_url: Some(&provider_config.base_url),
        api_key_env: Some(&provider_config.api_key_env),
    })?;
    println!("Job: {}", job.id);
    store.insert_segments(&job.id, &segments, prompt_version, &config.provider, &model)?;
    let run_config = TranslationRunConfig {
        source_language: config.source_language.clone(),
        target_language: config.target_language.clone(),
        provider: config.provider.clone(),
        model: model.clone(),
        prompt_version: prompt_version.to_string(),
        temperature: 0.2,
        scheduler: SchedulerConfig {
            concurrency: config.concurrency,
            max_retries: 3,
        },
    };
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
        },
    )?;
    let pending_segments = pending_segments_for_job(&store, &job.id, &segments)?;
    let fresh_translations = translate_and_checkpoint(
        provider.clone(),
        &pending_segments,
        &run_config,
        CheckpointContext {
            store: &store,
            job_id: &job.id,
            provider: &config.provider,
            model: &model,
            prompt_version,
        },
    )
    .await?;
    translations.extend(fresh_translations);
    translations.sort_by_key(|translation| translation.ordinal);
    let qa_reviews =
        qa_reviews_for_mode(provider, &segments, &translations, &run_config, qa_mode).await;
    mark_job_finished(&store, &job.id, &translations)?;
    print_summary_rebuild_and_report(
        &store,
        &job,
        &book,
        &segments,
        &translations,
        &qa_reviews,
        config,
    )?;

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
            mark_all_segments_failed(store, job_id, segments, &message)?;
            Err(anyhow::anyhow!(message))
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct CheckpointContext<'a> {
    pub store: &'a JobStore,
    pub job_id: &'a str,
    pub provider: &'a str,
    pub model: &'a str,
    pub prompt_version: &'a str,
}

#[derive(Clone, Copy)]
pub(crate) struct CacheContext<'a> {
    pub store: &'a JobStore,
    pub job_id: &'a str,
    pub prompt_version: &'a str,
    pub provider: &'a str,
    pub model: &'a str,
    pub source_lang: Option<&'a str>,
    pub target_lang: &'a str,
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
    match translate_segments_with_callback(provider, segments, config, |translation| {
        save_translation_result(
            checkpoint.store,
            checkpoint.job_id,
            translation,
            checkpoint.provider,
            checkpoint.model,
            checkpoint.prompt_version,
        )
        .map_err(|err| LlmError::Provider(format!("checkpoint save failed: {err}")))
    })
    .await
    {
        Ok(translations) => Ok(translations),
        Err(error) => {
            let message = format!(
                "translation scheduler failed before producing per-segment results: {error}"
            );
            mark_all_segments_failed(checkpoint.store, checkpoint.job_id, segments, &message)?;
            Err(anyhow::anyhow!(message))
        }
    }
}

pub(crate) fn apply_cached_translations(
    segments: &[Segment],
    cache: CacheContext<'_>,
) -> Result<Vec<SegmentTranslation>> {
    let mut cached = Vec::new();
    for segment in segments {
        let Some(hit) = cache.store.find_cached_translation(
            segment,
            cache.prompt_version,
            cache.provider,
            cache.model,
            cache.source_lang,
            cache.target_lang,
        )?
        else {
            continue;
        };
        cache.store.save_cached_translation(SaveCachedTranslation {
            job_id: cache.job_id,
            segment_id: &segment.id.0,
            translated_text: &hit.translated_text,
            blocks: &hit.blocks,
            provider: cache.provider,
            model: cache.model,
            prompt_version: cache.prompt_version,
        })?;
        cached.push(SegmentTranslation {
            segment_id: segment.id.clone(),
            ordinal: segment.ordinal,
            block_ids: segment.block_ids.clone(),
            blocks: hit.blocks,
            checksum: segment.checksum.clone(),
            status: SegmentStatus::SkippedCached,
            template: "cached".to_string(),
            error: None,
            input_tokens: None,
            output_tokens: None,
        });
    }
    Ok(cached)
}

pub(crate) fn pending_segments_for_job(
    store: &JobStore,
    job_id: &str,
    segments: &[Segment],
) -> Result<Vec<Segment>> {
    let pending_ids = store.pending_segment_ids(job_id)?;
    let pending = pending_ids
        .iter()
        .map(String::as_str)
        .collect::<std::collections::HashSet<_>>();
    Ok(segments
        .iter()
        .filter(|segment| pending.contains(segment.id.0.as_str()))
        .cloned()
        .collect())
}

pub(crate) async fn qa_reviews_for_mode<P>(
    provider: P,
    segments: &[Segment],
    translations: &[SegmentTranslation],
    config: &TranslationRunConfig,
    qa_mode: QaMode,
) -> Vec<QaSegmentReview>
where
    P: LlmProvider,
{
    match qa_mode {
        QaMode::Off => Vec::new(),
        QaMode::All => qa_segments(provider, segments, translations, config).await,
        QaMode::Suspicious => {
            let candidates = suspicious_qa_candidates(segments, translations);
            qa_segments(provider, segments, &candidates, config).await
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
        .filter(|translation| translation.status == SegmentStatus::Succeeded)
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
                || !segment.constraints.preserve_markers.is_empty()
        })
        .cloned()
        .collect()
}

fn mark_all_segments_failed(
    store: &JobStore,
    job_id: &str,
    segments: &[Segment],
    error: &str,
) -> Result<()> {
    for segment in segments {
        store.mark_segment_failed(job_id, &segment.id.0, error)?;
    }
    Ok(())
}

pub(crate) fn save_translation_result(
    store: &JobStore,
    job_id: &str,
    translation: &SegmentTranslation,
    provider: &str,
    model: &str,
    prompt_version: &str,
) -> Result<()> {
    let joined = translation.joined_text();
    match translation.status {
        SegmentStatus::Succeeded => store.save_translation(SaveTranslation {
            job_id,
            segment_id: &translation.segment_id.0,
            translated_text: &joined,
            blocks: &translation.blocks,
            provider,
            model,
            prompt_version,
            input_tokens: translation.input_tokens,
            output_tokens: translation.output_tokens,
        })?,
        SegmentStatus::NeedsReview => store.save_needs_review(SaveNeedsReview {
            job_id,
            segment_id: &translation.segment_id.0,
            preserved_text: &joined,
            blocks: &translation.blocks,
            provider,
            model,
            prompt_version,
            error: translation
                .error
                .as_deref()
                .unwrap_or("translation requires review"),
        })?,
        SegmentStatus::Failed => store.mark_segment_failed(
            job_id,
            &translation.segment_id.0,
            translation.error.as_deref().unwrap_or("translation failed"),
        )?,
        _ => {}
    }
    Ok(())
}

pub(crate) fn mark_job_finished(
    store: &JobStore,
    job_id: &str,
    translations: &[SegmentTranslation],
) -> Result<()> {
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

pub(crate) fn block_translations(translations: &[SegmentTranslation]) -> Vec<BlockTranslation> {
    translations
        .iter()
        .flat_map(|translation| translation.blocks.iter().cloned())
        .collect()
}

pub(crate) fn print_summary_rebuild_and_report(
    store: &JobStore,
    job: &JobRecord,
    book: &bookforge_core::ir::Book,
    segments: &[bookforge_core::segment::Segment],
    translations: &[SegmentTranslation],
    qa_reviews: &[QaSegmentReview],
    config: &TranslationConfig,
) -> Result<()> {
    let block_translations = block_translations(translations);
    rebuild_epub(book, &block_translations, &config.output)?;
    let summary = store
        .summary(&job.id)?
        .ok_or_else(|| anyhow::anyhow!("job '{}' was not found after translation", job.id))?;
    let segment_records = store.segment_records(&job.id)?;
    let report = write_report(ReportInput {
        job,
        summary: &summary,
        segments,
        segment_records: &segment_records,
        translations,
        qa_reviews,
        output: &config.output,
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
    println!("Output: {}", config.output.display());
    println!("Report: {}", report.markdown.display());

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookforge_core::{
        ir::{BlockId, SectionId},
        segment::{
            SegmentBlock, SegmentConstraints, SegmentContext, SegmentId, SegmentMetadata,
            SegmentSource, SegmentTextRun,
        },
    };
    use std::{fs, time::SystemTime};

    #[tokio::test]
    async fn scheduler_guard_marks_all_segments_failed_only_on_run_level_error() {
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
            })
            .expect("job should be created");
        let segments = vec![segment("seg_a", 0), segment("seg_b", 1)];
        store
            .insert_segments(&job.id, &segments, "v1", "mock", "mock-prefix")
            .expect("segments should insert");
        let config = TranslationRunConfig {
            source_language: Some("English".to_string()),
            target_language: "Italian".to_string(),
            provider: "mock".to_string(),
            model: "mock-prefix".to_string(),
            prompt_version: "v1".to_string(),
            temperature: 0.2,
            scheduler: SchedulerConfig {
                concurrency: 0,
                max_retries: 1,
            },
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
        assert_eq!(summary.failed, 2);
        assert_eq!(summary.succeeded, 0);

        let _ = fs::remove_file(db_path);
        let _ = fs::remove_file(input_path);
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
}
