use anyhow::Result;
use bookforge_core::{
    config::{SegmentationConfig, TranslationConfig},
    scheduler::SchedulerConfig,
    segment::build_segments,
};
use bookforge_epub::read_epub;
use bookforge_llm::{
    MockMode, MockProvider, OpenAiCompatibleConfig, OpenAiCompatibleProvider, TranslationRunConfig,
    translate_segments,
};
use bookforge_store::JobStore;
use clap::Args;
use std::fs;
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
    let prompt_version = "translate_segment.v1";
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
    let provider = MockProvider::new(MockMode::PrefixTarget, &config.target_language);
    let translations = match translate_segments(provider, &segments, &run_config).await {
        Ok(translations) => translations,
        Err(error) => {
            for segment in &segments {
                store.mark_segment_failed(&job.id, &segment.id.0, &error.to_string())?;
            }
            return Err(error.into());
        }
    };
    for translation in &translations {
        store.save_translation(
            &job.id,
            &translation.segment_id.0,
            &translation.text,
            "mock",
            &model,
            prompt_version,
            translation.input_tokens,
            translation.output_tokens,
        )?;
    }
    store.mark_job_complete(&job.id)?;
    let input_tokens = translations
        .iter()
        .filter_map(|translation| translation.input_tokens)
        .sum::<u64>();
    let output_tokens = translations
        .iter()
        .filter_map(|translation| translation.output_tokens)
        .sum::<u64>();

    fs::copy(input, &config.output)?;

    println!(
        "Translated: {}/{} segments",
        translations.len(),
        segments.len()
    );
    println!("Input tokens: {input_tokens}");
    println!("Output tokens: {output_tokens}");
    println!("Output: {}", config.output.display());
    println!("Mock mode copied the source EPUB; DOM patching arrives in Milestone 9.");

    Ok(())
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
    let prompt_version = "translate_segment.v1";
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
    let translations = match translate_segments(provider, &segments, &run_config).await {
        Ok(translations) => translations,
        Err(error) => {
            for segment in &segments {
                store.mark_segment_failed(&job.id, &segment.id.0, &error.to_string())?;
            }
            return Err(error.into());
        }
    };

    for translation in &translations {
        store.save_translation(
            &job.id,
            &translation.segment_id.0,
            &translation.text,
            &config.provider,
            &model,
            prompt_version,
            translation.input_tokens,
            translation.output_tokens,
        )?;
    }
    store.mark_job_complete(&job.id)?;
    fs::copy(input, &config.output)?;

    println!(
        "Translated: {}/{} segments",
        translations.len(),
        segments.len()
    );
    println!("Output: {}", config.output.display());
    println!("OpenAI-compatible translation is stored; DOM patching arrives in Milestone 9.");

    Ok(())
}
