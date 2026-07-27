use anyhow::Result;
use bookforge_core::config::{SegmentationConfig, TranslationProfile};
use bookforge_core::segment::{SEGMENT_UNIT_NAME, build_segments};
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

    /// Translation profile whose scheduler segmentation should be estimated.
    #[arg(long, value_enum, default_value_t = TranslationProfile::V1Fast)]
    pub profile: TranslationProfile,

    /// Override the profile's maximum scheduler-segment size.
    #[arg(long)]
    pub max_segment_tokens: Option<usize>,

    /// Override the profile's adjacent-segment context size.
    #[arg(long)]
    pub context_tokens: Option<usize>,

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
    profile: TranslationProfile,
    max_segment_tokens: usize,
    context_tokens: usize,
}

/// Estimate a run using the same default profile as `bookforge translate`.
///
/// The dashboard calls this entry point without exposing profile controls, so
/// keep its default in lockstep with the translate command's CLI default.
pub(crate) fn estimate_epub(
    input: &Path,
    target_language: &str,
    provider: &str,
    model: Option<&str>,
    pricing_path: Option<&Path>,
) -> Result<EstimateResult> {
    let profile = TranslationProfile::V1Fast;
    let segmentation = resolve_estimate_segmentation(profile, target_language, None, None);
    estimate_epub_with_segmentation(
        input,
        target_language,
        provider,
        model,
        pricing_path,
        profile,
        &segmentation,
    )
}

fn estimate_epub_with_segmentation(
    input: &Path,
    target_language: &str,
    provider: &str,
    model: Option<&str>,
    pricing_path: Option<&Path>,
    profile: TranslationProfile,
    segmentation: &SegmentationConfig,
) -> Result<EstimateResult> {
    let book = read_epub(input)?;
    let sizing = bookforge_core::style::built_in_sizing_policy_for_target(target_language);
    let segments = build_segments(&book, segmentation)?;
    let input_tokens = segments
        .iter()
        .map(|segment| segment.source.token_estimate as u64)
        .sum::<u64>();
    let output_tokens = sizing.map_or_else(
        || (input_tokens as f64 * 1.15).ceil() as u64,
        |policy| input_tokens.saturating_mul(policy.output_token_multiplier as u64),
    );
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
        profile,
        max_segment_tokens: segmentation.max_segment_tokens,
        context_tokens: segmentation.context_tokens,
    })
}

fn resolve_estimate_segmentation(
    profile: TranslationProfile,
    target_language: &str,
    max_segment_tokens: Option<usize>,
    context_tokens: Option<usize>,
) -> SegmentationConfig {
    let mut segmentation = profile.resolve().segmentation;
    if let Some(policy) = bookforge_core::style::built_in_sizing_policy_for_target(target_language)
    {
        segmentation.max_segment_tokens = segmentation
            .max_segment_tokens
            .min(policy.max_segment_tokens);
    }
    if let Some(max_segment_tokens) = max_segment_tokens {
        segmentation.max_segment_tokens = max_segment_tokens;
    }
    if let Some(context_tokens) = context_tokens {
        segmentation.context_tokens = context_tokens;
    }
    segmentation
}

pub async fn run(args: EstimateArgs) -> Result<()> {
    let segmentation = resolve_estimate_segmentation(
        args.profile,
        &args.language.target,
        args.max_segment_tokens,
        args.context_tokens,
    );
    let result = estimate_epub_with_segmentation(
        &args.input,
        &args.language.target,
        &args.provider.provider,
        args.provider.model.as_deref(),
        args.pricing.as_deref(),
        args.profile,
        &segmentation,
    )?;

    println!("Input: {}", args.input.display());
    println!("Target: {}", args.language.target);
    println!("Provider: {}", args.provider.provider);
    println!("Model: {}", result.model);
    println!("Profile: {:?}", result.profile);
    println!(
        "Max scheduler-segment tokens: {}",
        result.max_segment_tokens
    );
    println!("Context tokens: {}", result.context_tokens);
    println!(
        "Scheduler segments ({}): {}",
        SEGMENT_UNIT_NAME, result.segments
    );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, clap::Parser)]
    struct EstimateCli {
        #[command(flatten)]
        args: EstimateArgs,
    }

    #[derive(Debug, clap::Parser)]
    struct TranslateCli {
        #[command(flatten)]
        args: crate::commands::translate::TranslateArgs,
    }

    #[test]
    fn default_estimate_segmentation_matches_default_translation_profile() {
        let estimate =
            resolve_estimate_segmentation(TranslationProfile::V1Fast, "Italian", None, None);
        let scheduler = TranslationProfile::V1Fast.resolve().segmentation;

        assert_eq!(estimate.max_segment_tokens, scheduler.max_segment_tokens);
        assert_eq!(estimate.context_tokens, scheduler.context_tokens);
        assert_eq!(estimate.max_segment_tokens, 12_000);
    }

    #[test]
    fn estimate_and_translate_cli_defaults_select_the_same_profile() {
        use clap::Parser as _;

        let estimate =
            EstimateCli::try_parse_from(["estimate", "book.epub", "--target", "Italian"])
                .expect("estimate arguments should parse");
        let translate =
            TranslateCli::try_parse_from(["translate", "book.epub", "--target", "Italian"])
                .expect("translate arguments should parse");

        assert_eq!(estimate.args.profile, translate.args.profile);
    }

    #[test]
    fn explicit_estimate_overrides_follow_translation_precedence() {
        let estimate = resolve_estimate_segmentation(
            TranslationProfile::V1Fast,
            "Toki Pona",
            Some(777),
            Some(33),
        );

        assert_eq!(estimate.max_segment_tokens, 777);
        assert_eq!(estimate.context_tokens, 33);
    }
}
