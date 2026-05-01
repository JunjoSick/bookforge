use anyhow::Result;
use bookforge_core::{
    config::{SegmentationConfig, TranslationConfig},
    scheduler::SchedulerConfig,
    segment::{BlockTranslation, Segment, SegmentStatus, build_segments},
};
use bookforge_epub::{read_epub, rebuild_epub};
use bookforge_llm::{
    LlmProvider, MockMode, MockProvider, OpenAiCompatibleConfig, OpenAiCompatibleProvider,
    SegmentTranslation, TranslationRunConfig, translate_segments,
};
use bookforge_store::JobStore;
use clap::Args;
use std::path::PathBuf;

use crate::{LanguageArgs, ProviderArgs, default_output_path};

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
        "mock" => run_mock_translation(&args.input, &config).await?,
        "deepseek" | "openai-compatible" => {
            run_openai_compatible_translation(&args.input, &config, &args.provider).await?
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

async fn run_mock_translation(input: &PathBuf, config: &TranslationConfig) -> Result<()> {
    let book = read_epub(input)?;
    let segments = build_segments(&book, &SegmentationConfig::default())?;
    let model = config
        .model
        .clone()
        .unwrap_or_else(|| "mock-prefix-target".to_string());
    let prompt_version = "v1";
    let store = JobStore::open_default()?;
    let job = store.create_job(
        input,
        config.source_language.as_deref(),
        &config.target_language,
        "mock",
        &model,
    )?;
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
    let translations =
        translate_with_scheduler_guard(provider, &store, &job.id, &segments, &run_config).await?;
    for translation in &translations {
        save_translation_result(&store, &job.id, translation, "mock", &model, prompt_version)?;
    }
    mark_job_finished(&store, &job.id, &translations)?;
    print_summary_and_rebuild(&book, &segments, &translations, config)?;

    Ok(())
}

fn mock_mode(model: &str) -> MockMode {
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
) -> Result<()> {
    let provider_config = if config.provider == "deepseek" {
        OpenAiCompatibleConfig::deepseek(config.model.clone())
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
            timeout_seconds: 120,
        }
    };
    let provider = OpenAiCompatibleProvider::new(provider_config.clone())?;
    let model = provider.model().to_string();
    let book = read_epub(input)?;
    let segments = build_segments(&book, &SegmentationConfig::default())?;
    let prompt_version = "v1";
    let store = JobStore::open_default()?;
    let job = store.create_job(
        input,
        config.source_language.as_deref(),
        &config.target_language,
        &config.provider,
        &model,
    )?;
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
    let translations =
        translate_with_scheduler_guard(provider, &store, &job.id, &segments, &run_config).await?;

    for translation in &translations {
        save_translation_result(
            &store,
            &job.id,
            translation,
            &config.provider,
            &model,
            prompt_version,
        )?;
    }
    mark_job_finished(&store, &job.id, &translations)?;
    print_summary_and_rebuild(&book, &segments, &translations, config)?;

    Ok(())
}

async fn translate_with_scheduler_guard<P>(
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

fn save_translation_result(
    store: &JobStore,
    job_id: &str,
    translation: &SegmentTranslation,
    provider: &str,
    model: &str,
    prompt_version: &str,
) -> Result<()> {
    let joined = translation.joined_text();
    match translation.status {
        SegmentStatus::Succeeded => store.save_translation(
            job_id,
            &translation.segment_id.0,
            &joined,
            provider,
            model,
            prompt_version,
            translation.input_tokens,
            translation.output_tokens,
        )?,
        SegmentStatus::NeedsReview => store.save_needs_review(
            job_id,
            &translation.segment_id.0,
            &joined,
            provider,
            model,
            prompt_version,
            translation
                .error
                .as_deref()
                .unwrap_or("translation requires review"),
        )?,
        SegmentStatus::Failed => store.mark_segment_failed(
            job_id,
            &translation.segment_id.0,
            translation.error.as_deref().unwrap_or("translation failed"),
        )?,
        _ => {}
    }
    Ok(())
}

fn mark_job_finished(
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

fn block_translations(translations: &[SegmentTranslation]) -> Vec<BlockTranslation> {
    translations
        .iter()
        .flat_map(|translation| translation.blocks.iter().cloned())
        .collect()
}

fn print_summary_and_rebuild(
    book: &bookforge_core::ir::Book,
    segments: &[bookforge_core::segment::Segment],
    translations: &[SegmentTranslation],
    config: &TranslationConfig,
) -> Result<()> {
    let succeeded = translations
        .iter()
        .filter(|translation| translation.status == SegmentStatus::Succeeded)
        .count();
    let needs_review = translations
        .iter()
        .filter(|translation| translation.status == SegmentStatus::NeedsReview)
        .count();
    let failed = translations
        .iter()
        .filter(|translation| translation.status == SegmentStatus::Failed)
        .count();
    let input_tokens = translations
        .iter()
        .filter_map(|translation| translation.input_tokens)
        .sum::<u64>();
    let output_tokens = translations
        .iter()
        .filter_map(|translation| translation.output_tokens)
        .sum::<u64>();

    let block_translations = block_translations(translations);
    rebuild_epub(book, &block_translations, &config.output)?;

    println!("Translated: {}/{} segments", succeeded, segments.len());
    println!("Needs review: {needs_review}");
    println!("Failed: {failed}");
    println!("Input tokens: {input_tokens}");
    println!("Output tokens: {output_tokens}");
    println!("Output: {}", config.output.display());

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookforge_core::{
        ir::{BlockId, SectionId},
        segment::{
            SegmentBlock, SegmentConstraints, SegmentContext, SegmentId, SegmentMetadata,
            SegmentSource,
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
            .create_job(
                &input_path,
                Some("English"),
                "Italian",
                "mock",
                "mock-prefix",
            )
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
