use anyhow::Result;
use bookforge_core::config::SegmentationConfig;
use bookforge_core::segment::build_segments;
use bookforge_epub::read_epub;
use clap::Args;
use std::path::{Path, PathBuf};

use crate::{
    LanguageArgs, ProviderArgs,
    cost::{estimate_cost_usd_with_pricing, load_pricing},
};

#[derive(Debug, Args)]
pub struct EstimateArgs {
    pub input: PathBuf,

    #[command(flatten)]
    pub language: LanguageArgs,

    #[command(flatten)]
    pub provider: ProviderArgs,

    /// Override the bundled pricing catalog. BOOKFORGE_PRICING_PATH is
    /// used when this flag is omitted.
    #[arg(long)]
    pub pricing: Option<PathBuf>,
}

/// Token and cost estimate for translating an EPUB. Factored out of [`run`] so
/// both `bookforge estimate` and the web dashboard's `/api/estimate` share one
/// implementation (read EPUB → segment → sum token estimates → price).
#[derive(Debug, Clone)]
pub(crate) struct EstimateResult {
    pub(crate) segments: usize,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) model: String,
    pub(crate) cost_usd: Option<f64>,
    pub(crate) pricing_label: String,
}

pub(crate) fn estimate_epub(
    input: &Path,
    provider: &str,
    model: Option<&str>,
    pricing_path: Option<&Path>,
) -> Result<EstimateResult> {
    let book = read_epub(input)?;
    let segments = build_segments(&book, &SegmentationConfig::default())?;
    let input_tokens = segments
        .iter()
        .map(|segment| segment.source.token_estimate as u64)
        .sum::<u64>();
    let output_tokens = (input_tokens as f64 * 1.15).ceil() as u64;
    let model = model
        .map(str::to_string)
        .unwrap_or_else(|| default_model(provider).to_string());
    let pricing = load_pricing(pricing_path)?;
    let cost_usd =
        estimate_cost_usd_with_pricing(&pricing, provider, &model, input_tokens, 0, output_tokens);

    Ok(EstimateResult {
        segments: segments.len(),
        input_tokens,
        output_tokens,
        model,
        cost_usd,
        pricing_label: pricing.source_label(),
    })
}

pub async fn run(args: EstimateArgs) -> Result<()> {
    let result = estimate_epub(
        &args.input,
        &args.provider.provider,
        args.provider.model.as_deref(),
        args.pricing.as_deref(),
    )?;

    println!("Input: {}", args.input.display());
    println!("Target: {}", args.language.target);
    println!("Provider: {}", args.provider.provider);
    println!("Model: {}", result.model);
    println!("Segments: {}", result.segments);
    println!("Estimated input tokens: {}", result.input_tokens);
    println!("Estimated output tokens: {}", result.output_tokens);
    println!("Pricing: {}", result.pricing_label);

    match result.cost_usd {
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
