use anyhow::Result;
use bookforge_core::{
    config::{SegmentationConfig, TranslationConfig},
    scheduler::SchedulerConfig,
    segment::build_segments,
};
use bookforge_epub::read_epub;
use bookforge_llm::{MockMode, MockProvider, TranslationRunConfig, translate_segments};
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
        provider: args.provider.provider,
        model: args.provider.model,
        concurrency: args.concurrency,
        output,
    };

    println!("Input: {}", args.input.display());
    println!("Output: {}", config.output.display());
    println!("Target: {}", config.target_language);
    println!("Provider: {}", config.provider);
    println!("Concurrency: {}", config.concurrency);

    if config.provider == "mock" {
        run_mock_translation(&args.input, &config).await?;
    } else {
        println!(
            "Translation provider '{}' is not implemented yet.",
            config.provider
        );
    }

    Ok(())
}

async fn run_mock_translation(input: &PathBuf, config: &TranslationConfig) -> Result<()> {
    let book = read_epub(input)?;
    let segments = build_segments(&book, &SegmentationConfig::default())?;
    let run_config = TranslationRunConfig {
        source_language: config.source_language.clone(),
        target_language: config.target_language.clone(),
        provider: "mock".to_string(),
        model: config
            .model
            .clone()
            .unwrap_or_else(|| "mock-prefix-target".to_string()),
        prompt_version: "translate_segment.v1".to_string(),
        temperature: 0.2,
        scheduler: SchedulerConfig {
            concurrency: config.concurrency,
            max_retries: 3,
        },
    };
    let provider = MockProvider::new(MockMode::PrefixTarget, &config.target_language);
    let translations = translate_segments(provider, &segments, &run_config).await?;
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
