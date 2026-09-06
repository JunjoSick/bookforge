use anyhow::Result;
use bookforge_core::config::{DoubleCheckMode, SegmentationConfig, TranslationProfile};
use bookforge_core::providers::default_model_id;
use bookforge_core::segment::{SEGMENT_UNIT_NAME, build_segments};
use bookforge_epub::read_epub;
use clap::Args;
use std::path::{Path, PathBuf};

use crate::{
    LanguageArgs, ProviderArgs,
    cost::{estimate_cost_usd_with_pricing, load_pricing},
};

#[derive(Debug, Args)]
#[command(
    after_help = "Environment:\n  BOOKFORGE_PRICING_PATH  Override the bundled provider pricing table with a JSON file."
)]
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

    /// Override the bundled JSON pricing catalog. BOOKFORGE_PRICING_PATH is
    /// used when this flag is omitted.
    #[arg(long)]
    pub pricing: Option<PathBuf>,

    /// Append an approximate pass-cost breakdown (QA review, double-check,
    /// repair re-runs) beneath the primary estimate and print a real
    /// estimated total (primary + pass surcharges). Breakdown values are
    /// planning heuristics, never metered spend.
    #[arg(long)]
    pub pass_costs: bool,

    /// Double-check audit passes modeled in --pass-costs, overriding the
    /// count derived from the profile's double-check mode (Off resolves to 0).
    #[arg(long)]
    pub double_check_passes: Option<u32>,

    /// Fraction of segments assumed to require batch repair re-runs in
    /// --pass-costs. No measured failure rate exists yet; 0.05 is a
    /// conservative planning placeholder to calibrate per corpus/model.
    #[arg(long)]
    pub repair_share: Option<f64>,
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
#[cfg(feature = "serve")]
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
        .unwrap_or_else(|| default_model_id(provider).to_string());
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
        Some(cost) => println!("Estimated cost: ${cost:.4}"),
        None => println!("Estimated cost: unavailable for this provider/model"),
    }

    if args.pass_costs {
        print_pass_cost_breakdown(&args, &result)?;
    }

    Ok(())
}

/// Pass-cost planning rates.
///
/// Pass counts derive from the profile tables in `config.rs`: every built-in
/// profile ships a [`bookforge_core::QaRunConfig`] segment review (1 QA pass)
/// and the resolved `DoubleCheckConfig` mode maps Off→0 / audit mode→1
/// double-check pass. Token shapes are approximations (a QA or double-check
/// prompt re-reads source plus draft plus a fixed rubric and answers with
/// verdict JSON), so every value below is surfaced in the printed breakdown —
/// nothing is hidden inside a constant.
const QA_PASSES: f64 = 1.0;
const QA_INPUT_MULTIPLIER: f64 = 1.25;
const QA_OUTPUT_MULTIPLIER: f64 = 0.20;
const DOUBLE_CHECK_INPUT_MULTIPLIER: f64 = 1.50;
const DOUBLE_CHECK_OUTPUT_MULTIPLIER: f64 = 0.15;
const REPAIR_INPUT_MULTIPLIER: f64 = 1.0;
const REPAIR_OUTPUT_MULTIPLIER: f64 = 1.0;
pub(crate) const DEFAULT_REPAIR_SHARE: f64 = 0.05;

fn double_check_passes_for_profile(
    profile: TranslationProfile,
    override_passes: Option<u32>,
) -> u32 {
    override_passes.unwrap_or(match profile.resolve().double_check.mode {
        DoubleCheckMode::Off => 0,
        DoubleCheckMode::Formatting | DoubleCheckMode::Semantic | DoubleCheckMode::Full => 1,
    })
}

#[derive(Debug, Clone, PartialEq)]
struct PassCostRow {
    label: &'static str,
    input_tokens: u64,
    output_tokens: u64,
    usd: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
struct PassCostBreakdown {
    double_check_passes: u32,
    repair_share: f64,
    rows: Vec<PassCostRow>,
    /// Sum of the per-pass surcharges only. This is never a run total on its
    /// own: the printed total adds it to the primary estimate.
    surcharge_total_usd: Option<f64>,
}

/// Deterministic token sizing only; USD is attached per catalog afterwards so
/// tests do not depend on bundled prices. Rows with zero tokens are never
/// emitted: an empty or degenerate input must not print zero-token rows.
fn size_pass_tokens(
    base_input_tokens: u64,
    base_output_tokens: u64,
    double_check_passes: u32,
    repair_share: f64,
) -> Vec<(&'static str, u64, u64)> {
    let mut rows = Vec::new();
    let qa_input = ((base_input_tokens as f64) * QA_INPUT_MULTIPLIER * QA_PASSES).ceil() as u64;
    let qa_output = ((base_output_tokens as f64) * QA_OUTPUT_MULTIPLIER * QA_PASSES).ceil() as u64;
    if qa_input > 0 || qa_output > 0 {
        rows.push(("qa review", qa_input, qa_output));
    }
    if double_check_passes > 0 {
        let passes = double_check_passes as f64;
        let input =
            ((base_input_tokens as f64) * DOUBLE_CHECK_INPUT_MULTIPLIER * passes).ceil() as u64;
        let output =
            ((base_output_tokens as f64) * DOUBLE_CHECK_OUTPUT_MULTIPLIER * passes).ceil() as u64;
        if input > 0 || output > 0 {
            rows.push(("double-check", input, output));
        }
    }
    if repair_share > 0.0 {
        let input =
            ((base_input_tokens as f64) * REPAIR_INPUT_MULTIPLIER * repair_share).round() as u64;
        let output =
            ((base_output_tokens as f64) * REPAIR_OUTPUT_MULTIPLIER * repair_share).round() as u64;
        if input > 0 || output > 0 {
            rows.push(("repair share", input, output));
        }
    }
    rows
}

fn build_pass_cost_breakdown(
    pricing: &bookforge_core::providers::PricingCatalog,
    provider: &str,
    model: &str,
    base_input_tokens: u64,
    base_output_tokens: u64,
    double_check_passes: u32,
    repair_share: f64,
) -> PassCostBreakdown {
    let sized = size_pass_tokens(
        base_input_tokens,
        base_output_tokens,
        double_check_passes,
        repair_share,
    );
    let mut rows = Vec::with_capacity(sized.len());
    for (label, input_tokens, output_tokens) in sized {
        rows.push(PassCostRow {
            label,
            input_tokens,
            output_tokens,
            usd: estimate_cost_usd_with_pricing(
                pricing,
                provider,
                model,
                input_tokens,
                0,
                output_tokens,
            ),
        });
    }
    let usd_rows = rows.iter().filter_map(|row| row.usd);
    // A surcharge total only exists when every pass itself priced: a row
    // pricing as unavailable must not silently shrink the printed total.
    let surcharge_total_usd =
        (!rows.is_empty() && rows.iter().all(|row| row.usd.is_some())).then(|| usd_rows.sum());
    PassCostBreakdown {
        double_check_passes,
        repair_share,
        rows,
        surcharge_total_usd,
    }
}

/// One pass label plus its planning surcharge (`None` when that pass priced
/// as unavailable for the provider/model).
#[cfg(feature = "serve")]
pub(crate) type PassSurchargeList = Vec<(&'static str, Option<f64>)>;

/// Per-pass planning surcharges for an already-computed primary estimate,
/// plus the pass surcharge total. Shared by `estimate --pass-costs` (text)
/// and the dashboard's `/api/estimate` JSON so both surfaces never drift.
/// The surcharge total is `None` unless every pass priced.
#[cfg(feature = "serve")]
pub(crate) fn pass_cost_surcharges(
    provider: &str,
    result: &EstimateResult,
    pricing_path: Option<&Path>,
) -> Result<(PassSurchargeList, Option<f64>)> {
    let pricing = load_pricing(pricing_path)?;
    let breakdown = build_pass_cost_breakdown(
        &pricing,
        provider,
        &result.model,
        result.input_tokens,
        result.output_tokens,
        double_check_passes_for_profile(result.profile, None),
        DEFAULT_REPAIR_SHARE,
    );
    Ok((
        breakdown
            .rows
            .iter()
            .map(|row| (row.label, row.usd))
            .collect(),
        breakdown.surcharge_total_usd,
    ))
}

fn print_pass_cost_breakdown(args: &EstimateArgs, result: &EstimateResult) -> Result<()> {
    let double_check_passes =
        double_check_passes_for_profile(args.profile, args.double_check_passes);
    let repair_share = args
        .repair_share
        .unwrap_or(DEFAULT_REPAIR_SHARE)
        .clamp(0.0, 1.0);
    // Only the opt-in path pays a second catalog load; primary-output parity
    // with pre-breakdown invocations is kept byte-for-byte otherwise.
    let pricing = load_pricing(args.pricing.as_deref())?;
    let breakdown = build_pass_cost_breakdown(
        &pricing,
        &args.provider.provider,
        &result.model,
        result.input_tokens,
        result.output_tokens,
        double_check_passes,
        repair_share,
    );

    println!();
    println!("Pass-cost estimates (planning heuristics; not metered):");
    println!(
        "  assumptions: {} qa pass(es) @{:.2}x in/{:.2}x out | {:.0} double-check pass(es) @\
         {:.2}x in/{:.2}x out | repair share {:.2}",
        QA_PASSES,
        QA_INPUT_MULTIPLIER,
        QA_OUTPUT_MULTIPLIER,
        breakdown.double_check_passes,
        DOUBLE_CHECK_INPUT_MULTIPLIER,
        DOUBLE_CHECK_OUTPUT_MULTIPLIER,
        breakdown.repair_share,
    );
    for row in &breakdown.rows {
        println!("{}", format_pass_cost_row(row));
    }
    match (result.cost_usd, breakdown.surcharge_total_usd) {
        (Some(primary), Some(surcharges)) => {
            println!("Estimated total incl. passes: ${:.4}", primary + surcharges)
        }
        (Some(_), None) | (None, Some(_)) | (None, None) => {
            println!("Estimated total incl. passes: unavailable for this provider/model");
        }
    }
    Ok(())
}

/// One pass-cost line in the printed breakdown. The label column and token
/// columns are padded so every per-pass surcharge lands in the same column:
///
/// ```text
///   qa review    ~130337 in / ~23982 out  (+$0.0250)
///   repair share ~5213 in / ~5996 out     (+$0.0024)
/// ```
fn format_pass_cost_row(row: &PassCostRow) -> String {
    let tokens = format!("~{} in / ~{} out", row.input_tokens, row.output_tokens);
    let surcharge = match row.usd {
        Some(usd) => format!("(+${usd:.4})"),
        None => "(+unavailable)".to_string(),
    };
    format!("  {:<12} {tokens:<23}  {surcharge}", row.label)
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

    #[test]
    fn pass_cost_token_sizing_is_deterministic() {
        let rows = size_pass_tokens(1_000_000, 400_000, 0, 0.05);

        assert_eq!(
            rows,
            vec![
                ("qa review", 1_250_000, 80_000),
                ("repair share", 50_000, 20_000)
            ]
        );
    }

    #[test]
    fn double_check_override_models_extra_audit_passes() {
        let rows = size_pass_tokens(1_000_000, 400_000, 2, 0.0);

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "qa review");
        assert_eq!(rows[1], ("double-check", 3_000_000, 120_000));
    }

    #[test]
    fn empty_inputs_yield_no_pass_rows_for_every_pass_type() {
        // The zero-token-row guard must hold no matter which passes are
        // enabled: an empty input never prints a zero-token planning row.
        assert!(size_pass_tokens(0, 0, 0, 0.05).is_empty());
        assert!(size_pass_tokens(0, 0, 3, 0.05).is_empty());
        assert!(size_pass_tokens(0, 0, 0, 1.0).is_empty());
    }

    #[test]
    fn pass_cost_rows_render_with_aligned_surcharges() {
        let row = |label, input, output, usd| PassCostRow {
            label,
            input_tokens: input,
            output_tokens: output,
            usd,
        };
        let qa = format_pass_cost_row(&row("qa review", 130_337, 23_982, Some(0.025)));
        let repair = format_pass_cost_row(&row("repair share", 5_213, 5_996, Some(0.002_4)));
        let unavailable = format_pass_cost_row(&row("double-check", 1_234, 56, None));

        assert_eq!(qa, "  qa review    ~130337 in / ~23982 out  (+$0.0250)");
        assert_eq!(repair, "  repair share ~5213 in / ~5996 out     (+$0.0024)");
        assert_eq!(
            unavailable,
            "  double-check ~1234 in / ~56 out       (+unavailable)"
        );
        // Every surcharge lands in the same column.
        assert_eq!(qa.find("(+$"), repair.find("(+$"));
    }

    #[test]
    fn pass_cost_surcharge_total_requires_every_pass_to_price() {
        let pricing = load_pricing(None).expect("bundled pricing loads");
        let priced = build_pass_cost_breakdown(&pricing, "mock", "mock", 1_000, 400, 0, 0.05);
        assert!(priced.rows.iter().all(|row| row.usd == Some(0.0)));
        assert_eq!(priced.surcharge_total_usd, Some(0.0));

        // An unknown model prices no row, so no surcharge total exists and the
        // printed total cannot pretend one was computed.
        let unpriced = build_pass_cost_breakdown(&pricing, "mock", "mock", 1_000, 400, 0, 0.0);
        let no_catalog = build_pass_cost_breakdown(
            &pricing,
            "unknown-provider",
            "unknown-model",
            1_000,
            400,
            0,
            0.05,
        );
        assert!(no_catalog.rows.iter().all(|row| row.usd.is_none()));
        assert_eq!(no_catalog.surcharge_total_usd, None);
        assert_eq!(unpriced.rows.len(), 1, "qa review row still sizes tokens");
        assert_eq!(unpriced.rows[0].usd, Some(0.0));
        assert_eq!(unpriced.surcharge_total_usd, Some(0.0));
    }

    #[test]
    fn pass_cost_total_pins_primary_plus_surcharges() {
        // Fixed catalog so the pinned numbers do not move with bundled prices.
        let dir = tempfile::tempdir().expect("temp dir");
        let pricing_path = dir.path().join("pricing.json");
        std::fs::write(
            &pricing_path,
            r#"{
  "schema_version": 1,
  "updated_at": "2026-08-28T00:00:00Z",
  "providers": {
    "testprov": {
      "models": {
        "testmodel": {
          "input_per_million_usd": 2.0,
          "output_per_million_usd": 8.0,
          "input_cache_per_million_usd": null
        }
      }
    }
  }
}"#,
        )
        .expect("pricing fixture writes");
        let pricing = load_pricing(Some(&pricing_path)).expect("fixture pricing loads");

        // Primary: 100k in / 40k out -> 100000/1e6*2 + 40000/1e6*8 = $0.52.
        let primary =
            estimate_cost_usd_with_pricing(&pricing, "testprov", "testmodel", 100_000, 0, 40_000);
        assert_eq!(primary, Some(0.52));
        let breakdown =
            build_pass_cost_breakdown(&pricing, "testprov", "testmodel", 100_000, 40_000, 0, 0.05);

        let qa = breakdown
            .rows
            .iter()
            .find(|row| row.label == "qa review")
            .expect("qa row");
        // qa: 125000 in / 8000 out -> 0.25 + 0.064 = $0.314.
        assert!((qa.usd.expect("qa priced") - 0.314).abs() < 1e-12);
        let repair = breakdown
            .rows
            .iter()
            .find(|row| row.label == "repair share")
            .expect("repair row");
        // repair: 5000 in / 2000 out -> 0.01 + 0.016 = $0.026.
        assert!((repair.usd.expect("repair priced") - 0.026).abs() < 1e-12);

        let surcharges = breakdown.surcharge_total_usd.expect("every pass priced");
        assert!((surcharges - 0.34).abs() < 1e-9);
        let total = primary.expect("primary priced") + surcharges;
        // The printed total is a REAL total: primary + every surcharge.
        assert!(
            (total - 0.86).abs() < 1e-9,
            "total {total} must equal primary + surcharges"
        );
    }

    #[test]
    fn built_in_profiles_resolve_double_check_counts_from_config() {
        for profile in [
            TranslationProfile::Safe,
            TranslationProfile::Balanced,
            TranslationProfile::Fastest,
            TranslationProfile::FreeTier,
            TranslationProfile::TurboTextOnly,
            TranslationProfile::V1Fast,
        ] {
            assert_eq!(
                double_check_passes_for_profile(profile, None),
                0,
                "profile {profile:?} ships DoubleCheckMode::Off"
            );
            assert_eq!(
                double_check_passes_for_profile(profile, Some(3)),
                3,
                "explicit override must win over the profile mode"
            );
        }
    }
}
