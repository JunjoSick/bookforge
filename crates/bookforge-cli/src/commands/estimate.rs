use anyhow::Result;
use bookforge_core::config::SegmentationConfig;
use bookforge_core::segment::build_segments;
use bookforge_epub::read_epub;
use clap::Args;
use std::path::PathBuf;

use crate::{LanguageArgs, ProviderArgs, cost::estimate_cost_usd};

#[derive(Debug, Args)]
pub struct EstimateArgs {
    pub input: PathBuf,

    #[command(flatten)]
    pub language: LanguageArgs,

    #[command(flatten)]
    pub provider: ProviderArgs,
}

pub async fn run(args: EstimateArgs) -> Result<()> {
    let book = read_epub(&args.input)?;
    let segments = build_segments(&book, &SegmentationConfig::default())?;
    let input_tokens = segments
        .iter()
        .map(|segment| segment.source.token_estimate as u64)
        .sum::<u64>();
    let output_tokens = (input_tokens as f64 * 1.15).ceil() as u64;
    let model = args
        .provider
        .model
        .as_deref()
        .unwrap_or_else(|| default_model(&args.provider.provider));

    println!("Input: {}", args.input.display());
    println!("Target: {}", args.language.target);
    println!("Provider: {}", args.provider.provider);
    println!("Model: {model}");
    println!("Segments: {}", segments.len());
    println!("Estimated input tokens: {input_tokens}");
    println!("Estimated output tokens: {output_tokens}");

    match estimate_cost_usd(&args.provider.provider, model, input_tokens, output_tokens) {
        Some(cost) => println!("Estimated cost: ${cost:.6}"),
        None => println!("Estimated cost: unavailable for this provider/model"),
    }

    Ok(())
}

fn default_model(provider: &str) -> &str {
    match provider {
        "mock" => "mock-prefix-target",
        "deepseek" => "deepseek-v4-flash",
        "openrouter" => "openrouter/auto",
        _ => "unknown",
    }
}
