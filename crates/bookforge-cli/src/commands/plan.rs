use std::path::{Path, PathBuf};

use anyhow::Result;
use bookforge_core::{
    config::TranslationProfile,
    ir::{Block, BlockKind, Book},
    script::{ScriptClass, classify, script_counts},
    segment::{build_segments, estimate_tokens},
    style::built_in_sizing_policy_for_target,
};
use bookforge_epub::read_epub;
use bookforge_llm::{BatchMode, TranslationBatch, build_translation_batches};
use clap::Args;
use serde::Serialize;

pub(crate) const PLAN_SCHEMA_VERSION: u32 = 1;
// The smallest measured oversized-response failures needed about 9,000 output
// tokens. Use the power-of-two boundary immediately below that observed cliff.
const SAFE_RESPONSE_TOKENS: u32 = 8_192;
const JSON_RESPONSE_FIXED_TOKENS: usize = 128;
const JSON_RESPONSE_TOKENS_PER_ITEM: usize = 64;
const GENERIC_OUTPUT_NUMERATOR: usize = 115;
const GENERIC_OUTPUT_DENOMINATOR: usize = 100;

#[derive(Debug, Args)]
pub struct PlanArgs {
    /// EPUB to inspect. The file is never modified.
    pub input: PathBuf,

    /// Declared source language. Sizing is derived from the book text, not this value.
    #[arg(long)]
    pub source: Option<String>,

    /// Translation target language.
    #[arg(long)]
    pub target: String,

    /// Provider whose offline runtime limits should be used.
    #[arg(long, default_value = "deepseek")]
    pub provider: String,

    /// Model name, used only for offline reasoning-model classification.
    #[arg(long)]
    pub model: Option<String>,

    /// Emit stable, machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Plan {
    pub schema_version: u32,
    pub advisory_only: bool,
    pub input: String,
    pub source: SourceInspection,
    pub target: String,
    pub provider: ProviderInspection,
    pub inspection: BookInspection,
    pub recommendations: Recommendations,
    pub prior_runs: Evidence<bool>,
    pub warnings: Vec<String>,
    pub translate_flags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceInspection {
    pub declared_language: Option<String>,
    pub detected_script: ScriptClass,
    pub alphabetic_cased_characters: usize,
    pub alphabetic_caseless_characters: usize,
    pub sizing_uses_declared_language: bool,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderInspection {
    pub name: String,
    pub model: String,
    pub reasoning_model: bool,
    pub output_ceiling_tokens: Evidence<u32>,
    pub thinking_suppression_parameter: Evidence<Option<String>>,
    pub input_cache: Evidence<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BookInspection {
    pub source_characters: usize,
    pub estimated_source_tokens: usize,
    pub blocks: Distribution,
    pub block_estimated_output_tokens: Distribution,
    pub segments: Distribution,
    pub segment_block_counts: Distribution,
    pub default_batches: BatchInspection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BatchInspection {
    pub count: usize,
    pub estimated_output_tokens: Distribution,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Distribution {
    pub count: usize,
    pub total: usize,
    pub median: usize,
    pub p90: usize,
    pub max: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Recommendations {
    pub batch_target_tokens: Recommendation<usize>,
    pub batch_max_items: Recommendation<usize>,
    pub batch_max_output_tokens: Recommendation<Option<u32>>,
    pub max_output_tokens: Recommendation<u32>,
    pub no_thinking: Recommendation<bool>,
    pub concurrency: Recommendation<usize>,
    pub adaptive_concurrency: Recommendation<bool>,
    pub glossary_enabled: Recommendation<bool>,
}

impl Recommendations {
    fn all_have_reasons(&self) -> bool {
        [
            self.batch_target_tokens.reason.as_str(),
            self.batch_max_items.reason.as_str(),
            self.batch_max_output_tokens.reason.as_str(),
            self.max_output_tokens.reason.as_str(),
            self.no_thinking.reason.as_str(),
            self.concurrency.reason.as_str(),
            self.adaptive_concurrency.reason.as_str(),
            self.glossary_enabled.reason.as_str(),
        ]
        .into_iter()
        .all(|reason| !reason.trim().is_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Recommendation<T> {
    pub value: T,
    pub disposition: Disposition,
    pub flag: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Set,
    KeepDefault,
    Omit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Evidence<T> {
    pub value: T,
    pub reason: String,
}

pub async fn run(args: PlanArgs) -> Result<()> {
    let plan = create_plan(
        &args.input,
        args.source.as_deref(),
        &args.target,
        &args.provider,
        args.model.as_deref(),
    )?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&plan)?);
    } else {
        print_human(&plan);
    }
    Ok(())
}

fn create_plan(
    input: &Path,
    declared_source: Option<&str>,
    target: &str,
    provider: &str,
    model: Option<&str>,
) -> Result<Plan> {
    let book = read_epub(input)?;
    Ok(plan_book(
        &book,
        input,
        declared_source,
        target,
        provider,
        model,
    )?)
}

/// Build the same deterministic, offline plan used by the `plan` command from
/// an EPUB that has already been parsed. Integrated callers can therefore
/// reuse the in-memory book instead of parsing the archive a second time.
pub(crate) fn plan_book(
    book: &Book,
    input: &Path,
    declared_source: Option<&str>,
    target: &str,
    provider: &str,
    model: Option<&str>,
) -> bookforge_core::Result<Plan> {
    let mut settings = TranslationProfile::V1Fast.resolve();
    if let Some(policy) = built_in_sizing_policy_for_target(target) {
        settings.segmentation.max_segment_tokens = settings
            .segmentation
            .max_segment_tokens
            .min(policy.max_segment_tokens);
        settings.batch.target_tokens = settings.batch.target_tokens.min(policy.batch_target_tokens);
        settings.batch.max_items = settings.batch.max_items.min(policy.batch_max_items);
    }

    let translatable_blocks = book
        .blocks
        .iter()
        .filter(|block| {
            !matches!(block.kind, BlockKind::Code | BlockKind::PageFurniture)
                && !block_text(block).trim().is_empty()
        })
        .collect::<Vec<_>>();
    let source_text = translatable_blocks
        .iter()
        .map(|block| block_text(block))
        .collect::<Vec<_>>()
        .join("\n");
    let source_characters = source_text.chars().count();
    let (cased, caseless) = script_counts(&source_text);
    let detected_script = classify((cased, caseless));

    let block_tokens = translatable_blocks
        .iter()
        .map(|block| estimate_tokens(&block_text(block)).max(1))
        .collect::<Vec<_>>();
    let output_ratio = output_ratio(target);
    let block_output_tokens = translatable_blocks
        .iter()
        .zip(&block_tokens)
        .map(|(block, tokens)| estimated_item_output_tokens(block, *tokens, output_ratio))
        .collect::<Vec<_>>();

    let segments = build_segments(book, &settings.segmentation)?;
    let segment_tokens = segments
        .iter()
        .map(|segment| segment.source.token_estimate)
        .collect::<Vec<_>>();
    let segment_block_counts = segments
        .iter()
        .map(|segment| segment.source.blocks.len())
        .collect::<Vec<_>>();
    let default_batches =
        build_translation_batches(&segments, &settings.batch, TranslationProfile::V1Fast);
    let default_batch_outputs = default_batches
        .iter()
        .map(|batch| estimated_batch_output_tokens(batch, output_ratio))
        .collect::<Vec<_>>();

    let provider = inspect_provider(provider, model);
    let safe_response_tokens = provider
        .output_ceiling_tokens
        .value
        .min(SAFE_RESPONSE_TOKENS);
    let density_guard = source_density_guard(source_characters, block_tokens.iter().sum());
    let recommendations = recommend(
        &settings,
        target,
        detected_script,
        density_guard,
        &distribution(&block_tokens),
        &distribution(&block_output_tokens),
        &distribution(&default_batch_outputs),
        safe_response_tokens,
        &provider,
    );
    debug_assert!(recommendations.all_have_reasons());

    let mut warnings = Vec::new();
    let max_single_budget = translatable_blocks
        .iter()
        .zip(&block_tokens)
        .map(|(block, tokens)| provider_budget_for_block(block, *tokens, provider.reasoning_model))
        .max()
        .unwrap_or(0);
    if max_single_budget > provider.output_ceiling_tokens.value as usize {
        warnings.push(format!(
            "At least one block needs an estimated output budget of {max_single_budget} tokens, above BookForge's {}-token ceiling for this provider/model. Batching cannot split a single block; reflow or split that source block before translating if it truncates.",
            provider.output_ceiling_tokens.value
        ));
    }
    if block_tokens.iter().copied().max().unwrap_or(0) > settings.segmentation.max_segment_tokens {
        warnings.push(format!(
            "At least one block exceeds the {}-token scheduler-segment target. The current segmenter keeps individual blocks intact, so lowering --max-segment-tokens would not split it.",
            settings.segmentation.max_segment_tokens
        ));
    }

    let translate_flags = translate_flags(&recommendations);
    Ok(Plan {
        schema_version: PLAN_SCHEMA_VERSION,
        advisory_only: true,
        input: input.display().to_string(),
        source: SourceInspection {
            declared_language: declared_source.map(str::to_string),
            detected_script,
            alphabetic_cased_characters: cased,
            alphabetic_caseless_characters: caseless,
            sizing_uses_declared_language: false,
            reason: format!(
                "Dominant script is derived from the EPUB text itself ({cased} cased versus {caseless} caseless alphabetic characters); --source is recorded but never used for sizing."
            ),
        },
        target: target.to_string(),
        provider,
        inspection: BookInspection {
            source_characters,
            estimated_source_tokens: block_tokens.iter().sum(),
            blocks: distribution(&block_tokens),
            block_estimated_output_tokens: distribution(&block_output_tokens),
            segments: distribution(&segment_tokens),
            segment_block_counts: distribution(&segment_block_counts),
            default_batches: BatchInspection {
                count: default_batches.len(),
                estimated_output_tokens: distribution(&default_batch_outputs),
            },
        },
        recommendations,
        prior_runs: Evidence {
            value: false,
            reason: "This read-only slice does not open the working-directory job database, because the current store API opens and migrates it. No prior-run evidence was applied and no .bookforge directory was created."
                .to_string(),
        },
        warnings,
        translate_flags,
    })
}

#[allow(clippy::too_many_arguments)]
fn recommend(
    settings: &bookforge_core::config::ResolvedRunSettings,
    target: &str,
    script: ScriptClass,
    density_guard: usize,
    block_tokens: &Distribution,
    block_outputs: &Distribution,
    default_batch_outputs: &Distribution,
    safe_response_tokens: u32,
    provider: &ProviderInspection,
) -> Recommendations {
    let target_policy = built_in_sizing_policy_for_target(target);
    let default_target = settings.batch.target_tokens;
    let default_max_items = settings.batch.max_items;

    let (batch_target, batch_max_items, batch_reason, items_reason) = if target_policy.is_some() {
        (
            default_target,
            default_max_items,
            format!(
                "Keep the built-in {target} sizing policy; it is already stricter than the general script and tail rules."
            ),
            format!(
                "Keep the built-in {target} item bound; target-language expansion is the controlling constraint."
            ),
        )
    } else if script == ScriptClass::Caseless {
        let guarded_output_budget = (safe_response_tokens as usize / density_guard.max(1))
            .saturating_sub(JSON_RESPONSE_FIXED_TOKENS)
            .max(1);
        let max_items_from_p90 = guarded_output_budget
            .checked_div(block_outputs.p90.max(1))
            .unwrap_or(1)
            .clamp(1, default_max_items);
        let output_limited_source = guarded_output_budget
            .saturating_mul(GENERIC_OUTPUT_DENOMINATOR)
            .checked_div(GENERIC_OUTPUT_NUMERATOR)
            .unwrap_or(1);
        let p90_limited_source = block_tokens.p90.saturating_mul(max_items_from_p90);
        let raw_target = default_target
            .min(output_limited_source)
            .min(p90_limited_source.max(1));
        let batch_target = round_down(raw_target, 256).max(256).min(default_target);
        (
            batch_target,
            max_items_from_p90,
            format!(
                "The text is caseless-script dominant and its measured token density needs a {density_guard}x guard relative to cased prose. The target is the smaller of the profile default, the guarded {safe_response_tokens}-token response budget, and p90 block size ({}) times the item bound; it is rounded down to 256-token steps.",
                block_tokens.p90
            ),
            format!(
                "A guarded {safe_response_tokens}-token response fits {max_items_from_p90} p90 items at about {} output tokens each. This derives the item bound from the tail instead of copying the 800-token/4-item experiment.",
                block_outputs.p90
            ),
        )
    } else {
        (
            default_target,
            default_max_items,
            format!(
                "Keep the v1-fast default: script-aware estimates and the output-tail bound are sufficient for this {}-script source, so shrinking every request would only repeat prompt overhead.",
                script.label()
            ),
            format!(
                "Keep the v1-fast default: p90 item output is {} tokens, and any oversized tail is isolated by --batch-max-output-tokens rather than a global item-count reduction.",
                block_outputs.p90
            ),
        )
    };

    let target_disposition = if batch_target == default_target {
        Disposition::KeepDefault
    } else {
        Disposition::Set
    };
    let items_disposition = if batch_max_items == default_max_items {
        Disposition::KeepDefault
    } else {
        Disposition::Set
    };

    let needs_output_bound = default_batch_outputs.max > safe_response_tokens as usize;
    let batch_max_output_tokens = needs_output_bound.then_some(safe_response_tokens);
    let batch_output_reason = if needs_output_bound {
        format!(
            "The default batch tail reaches an estimated {} output tokens, beyond the {safe_response_tokens}-token safety boundary below the measured ~9,000-token failure cliff. The executor uses this cap while packing, so only oversized shapes split.",
            default_batch_outputs.max
        )
    } else {
        format!(
            "Leave the optional bound unset: the largest default batch is estimated at {} output tokens, below the {safe_response_tokens}-token safety boundary, so defaults are fine.",
            default_batch_outputs.max
        )
    };

    let suppression = provider.thinking_suppression_parameter.value.is_some();
    let suppression_reason = provider
        .thinking_suppression_parameter
        .value
        .as_deref()
        .map_or_else(
            || {
                "Do not request thinking suppression: the selected provider has no recognized parameter, and BookForge deliberately sends no guessed field."
                    .to_string()
            },
            |parameter| {
                format!(
                    "Keep reasoning off so translated content cannot be starved by hidden reasoning tokens; BookForge sends {parameter} for this provider."
                )
            },
        );

    Recommendations {
        batch_target_tokens: Recommendation {
            value: batch_target,
            disposition: target_disposition,
            flag: (target_disposition == Disposition::Set)
                .then(|| format!("--batch-target-tokens {batch_target}")),
            reason: batch_reason,
        },
        batch_max_items: Recommendation {
            value: batch_max_items,
            disposition: items_disposition,
            flag: (items_disposition == Disposition::Set)
                .then(|| format!("--batch-max-items {batch_max_items}")),
            reason: items_reason,
        },
        batch_max_output_tokens: Recommendation {
            value: batch_max_output_tokens,
            disposition: if needs_output_bound {
                Disposition::Set
            } else {
                Disposition::Omit
            },
            flag: batch_max_output_tokens
                .map(|tokens| format!("--batch-max-output-tokens {tokens}")),
            reason: batch_output_reason,
        },
        max_output_tokens: Recommendation {
            value: provider.output_ceiling_tokens.value,
            disposition: Disposition::Set,
            flag: Some(format!(
                "--max-output-tokens {}",
                provider.output_ceiling_tokens.value
            )),
            reason: format!(
                "Use BookForge's {}-token ceiling for this provider/model so a lower cap cannot let reasoning exhaust the budget before content is returned.",
                provider.output_ceiling_tokens.value
            ),
        },
        no_thinking: Recommendation {
            value: suppression,
            disposition: if suppression {
                Disposition::KeepDefault
            } else {
                Disposition::Omit
            },
            flag: suppression.then(|| "--no-thinking".to_string()),
            reason: suppression_reason,
        },
        concurrency: Recommendation {
            value: settings.scheduler.concurrency,
            disposition: Disposition::KeepDefault,
            flag: None,
            reason: format!(
                "Keep the v1-fast default of {}: this offline inspection has no latency or 429 observations from which to justify a different fixed value.",
                settings.scheduler.concurrency
            ),
        },
        adaptive_concurrency: Recommendation {
            value: settings.adaptive_concurrency,
            disposition: Disposition::KeepDefault,
            flag: None,
            reason: "Keep adaptive concurrency enabled so an actual run can react to latency and 429 evidence that an offline plan cannot observe."
                .to_string(),
        },
        glossary_enabled: Recommendation {
            value: false,
            disposition: Disposition::Omit,
            flag: None,
            reason: "Leave glossary injection off by default: the measured glossary A/B found no detectable quality effect."
                .to_string(),
        },
    }
}

fn inspect_provider(provider: &str, model: Option<&str>) -> ProviderInspection {
    let normalized = provider.trim().to_ascii_lowercase();
    let model = model
        .unwrap_or_else(|| default_model(&normalized))
        .to_string();
    let reasoning_model = model_name_is_reasoning(&model);
    let extended_output = normalized == "deepseek" || reasoning_model;
    let output_ceiling = if extended_output { 32_768 } else { 16_384 };
    let suppression = match normalized.as_str() {
        "openrouter" => Some("reasoning.enabled=false".to_string()),
        "openai" => Some("reasoning_effort=none".to_string()),
        "deepseek" => Some("thinking.type=disabled".to_string()),
        _ => None,
    };
    let suppression_reason = suppression.as_ref().map_or_else(
        || {
            "The provider name does not identify an endpoint for which BookForge has a documented suppression field."
                .to_string()
        },
        |parameter| {
            format!(
                "BookForge's provider routing recognizes this provider and sends {parameter} when thinking is disabled."
            )
        },
    );
    let cache_value = if normalized == "deepseek" {
        "provider_managed".to_string()
    } else {
        "unknown_offline".to_string()
    };
    let cache_reason = if normalized == "deepseek" {
        "DeepSeek caching is provider-managed; BookForge records cached-input usage when returned and sends no cache-control flag."
            .to_string()
    } else {
        "BookForge can record cached-input usage returned by an OpenAI-compatible response, but the code has no offline capability catalog proving that this provider/model caches input."
            .to_string()
    };

    ProviderInspection {
        name: normalized,
        model,
        reasoning_model,
        output_ceiling_tokens: Evidence {
            value: output_ceiling,
            reason: if extended_output {
                "The current executor permits up to 32,768 output tokens for DeepSeek or a model name it classifies as reasoning."
                    .to_string()
            } else {
                "The current executor permits up to 16,384 output tokens for other non-reasoning provider/model pairs."
                    .to_string()
            },
        },
        thinking_suppression_parameter: Evidence {
            value: suppression,
            reason: suppression_reason,
        },
        input_cache: Evidence {
            value: cache_value,
            reason: cache_reason,
        },
    }
}

fn default_model(provider: &str) -> &'static str {
    match provider {
        "mock" => "mock-prefix-target",
        "deepseek" => "deepseek-v4-flash",
        "openrouter" => "openrouter/auto",
        _ => "unknown",
    }
}

// Keep this in lockstep with the current provider's offline name heuristic.
fn model_name_is_reasoning(model: &str) -> bool {
    let lower = model.to_lowercase();
    lower.contains("reasoner")
        || lower.contains("v4-flash")
        || lower.starts_with("o1")
        || lower.starts_with("o3")
        || lower.starts_with("o4")
}

fn block_text(block: &Block) -> String {
    block
        .text_runs
        .iter()
        .map(|run| run.text.as_str())
        .collect::<Vec<_>>()
        .join("")
}

fn source_density_guard(source_characters: usize, source_tokens: usize) -> usize {
    if source_characters == 0 {
        return 1;
    }
    // Four characters per token is the conservative edge of ordinary cased
    // prose. This turns measured tokens-per-character into a whole-number
    // request-density guard: cased prose is 1x and Han-like prose is 4x.
    source_tokens
        .saturating_mul(4)
        .div_ceil(source_characters)
        .clamp(1, 4)
}

fn output_ratio(target: &str) -> (usize, usize) {
    built_in_sizing_policy_for_target(target).map_or(
        (GENERIC_OUTPUT_NUMERATOR, GENERIC_OUTPUT_DENOMINATOR),
        |policy| (policy.output_token_multiplier, 1),
    )
}

fn estimated_item_output_tokens(
    block: &Block,
    source_tokens: usize,
    output_ratio: (usize, usize),
) -> usize {
    let translated = source_tokens
        .saturating_mul(output_ratio.0)
        .div_ceil(output_ratio.1);
    let run_envelope = if block.text_runs.len() > 12 {
        block.text_runs.len().saturating_mul(16)
    } else {
        0
    };
    translated
        .saturating_add(JSON_RESPONSE_TOKENS_PER_ITEM)
        .saturating_add(run_envelope)
}

fn estimated_batch_output_tokens(batch: &TranslationBatch, output_ratio: (usize, usize)) -> usize {
    let source_tokens = batch
        .items
        .iter()
        .map(|item| estimate_tokens(&item.source_text).max(1))
        .sum::<usize>();
    let translated = source_tokens
        .saturating_mul(output_ratio.0)
        .div_ceil(output_ratio.1);
    let run_envelope = if batch.mode == BatchMode::RunPreserving {
        batch
            .items
            .iter()
            .map(|item| item.text_runs.len().saturating_mul(16))
            .sum()
    } else {
        0
    };
    translated
        .saturating_add(JSON_RESPONSE_FIXED_TOKENS)
        .saturating_add(
            batch
                .items
                .len()
                .saturating_mul(JSON_RESPONSE_TOKENS_PER_ITEM),
        )
        .saturating_add(run_envelope)
}

fn provider_budget_for_block(block: &Block, source_tokens: usize, reasoning: bool) -> usize {
    let base_multiplier = if block.text_runs.len() > 12 {
        5
    } else if !block.inline_marks.is_empty() || !block.protected_spans.is_empty() {
        4
    } else {
        3
    };
    let multiplier = if reasoning {
        base_multiplier * 3
    } else {
        base_multiplier
    };
    source_tokens
        .saturating_mul(multiplier)
        .saturating_add(JSON_RESPONSE_FIXED_TOKENS + JSON_RESPONSE_TOKENS_PER_ITEM)
}

fn distribution(values: &[usize]) -> Distribution {
    if values.is_empty() {
        return Distribution::default();
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    Distribution {
        count: sorted.len(),
        total: sorted.iter().sum(),
        median: nearest_rank(&sorted, 50),
        p90: nearest_rank(&sorted, 90),
        max: sorted.last().copied().unwrap_or(0),
    }
}

fn nearest_rank(sorted: &[usize], percentile: usize) -> usize {
    let rank = sorted.len().saturating_mul(percentile).div_ceil(100);
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

fn round_down(value: usize, step: usize) -> usize {
    value / step * step
}

fn translate_flags(recommendations: &Recommendations) -> Vec<String> {
    [
        recommendations.batch_target_tokens.flag.as_ref(),
        recommendations.batch_max_items.flag.as_ref(),
        recommendations.batch_max_output_tokens.flag.as_ref(),
        recommendations.max_output_tokens.flag.as_ref(),
        recommendations.no_thinking.flag.as_ref(),
    ]
    .into_iter()
    .flatten()
    .cloned()
    .collect()
}

fn print_human(plan: &Plan) {
    println!("BookForge translation plan (advisory only; nothing was run or changed)");
    println!("Input: {}", plan.input);
    println!(
        "Source: declared={}, detected script={}",
        plan.source.declared_language.as_deref().unwrap_or("(none)"),
        plan.source.detected_script.label()
    );
    println!("  Reason: {}", plan.source.reason);
    println!("Target: {}", plan.target);
    println!("Provider: {} / {}", plan.provider.name, plan.provider.model);
    println!(
        "Provider output ceiling: {} tokens",
        plan.provider.output_ceiling_tokens.value
    );
    println!("  Reason: {}", plan.provider.output_ceiling_tokens.reason);
    println!(
        "Thinking suppression: {}",
        plan.provider
            .thinking_suppression_parameter
            .value
            .as_deref()
            .unwrap_or("unsupported")
    );
    println!(
        "  Reason: {}",
        plan.provider.thinking_suppression_parameter.reason
    );
    println!("Input cache: {}", plan.provider.input_cache.value);
    println!("  Reason: {}", plan.provider.input_cache.reason);
    println!("Inspection:");
    println!(
        "  Source: {} characters, {} estimated tokens",
        plan.inspection.source_characters, plan.inspection.estimated_source_tokens
    );
    print_distribution("Block tokens", &plan.inspection.blocks);
    print_distribution(
        "Estimated output tokens per block",
        &plan.inspection.block_estimated_output_tokens,
    );
    print_distribution("Scheduler-segment tokens", &plan.inspection.segments);
    print_distribution(
        "Blocks per scheduler segment",
        &plan.inspection.segment_block_counts,
    );
    print_distribution(
        "Default-batch estimated output tokens",
        &plan.inspection.default_batches.estimated_output_tokens,
    );
    println!("Recommendations:");
    print_recommendation(
        "batch target tokens",
        &plan.recommendations.batch_target_tokens,
    );
    print_recommendation("batch max items", &plan.recommendations.batch_max_items);
    print_optional_recommendation(
        "batch max output tokens",
        &plan.recommendations.batch_max_output_tokens,
    );
    print_recommendation(
        "provider max output tokens",
        &plan.recommendations.max_output_tokens,
    );
    print_recommendation("thinking disabled", &plan.recommendations.no_thinking);
    print_recommendation("concurrency", &plan.recommendations.concurrency);
    print_recommendation(
        "adaptive concurrency",
        &plan.recommendations.adaptive_concurrency,
    );
    print_recommendation("glossary enabled", &plan.recommendations.glossary_enabled);
    println!("Prior runs consulted: {}", plan.prior_runs.value);
    println!("  Reason: {}", plan.prior_runs.reason);
    for warning in &plan.warnings {
        println!("Warning: {warning}");
    }
    if !plan.translate_flags.is_empty() {
        println!(
            "Recommended translate flags: {}",
            plan.translate_flags.join(" ")
        );
    }
}

fn print_distribution(label: &str, distribution: &Distribution) {
    println!(
        "  {label}: count={}, median={}, p90={}, max={}",
        distribution.count, distribution.median, distribution.p90, distribution.max
    );
}

fn print_recommendation<T: std::fmt::Display>(label: &str, recommendation: &Recommendation<T>) {
    println!(
        "  {label}: {} ({})",
        recommendation.value,
        disposition_label(recommendation.disposition)
    );
    println!("    Reason: {}", recommendation.reason);
}

fn print_optional_recommendation<T: std::fmt::Display>(
    label: &str,
    recommendation: &Recommendation<Option<T>>,
) {
    let value = recommendation
        .value
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_else(|| "unset".to_string());
    println!(
        "  {label}: {value} ({})",
        disposition_label(recommendation.disposition)
    );
    println!("    Reason: {}", recommendation.reason);
}

fn disposition_label(disposition: Disposition) -> &'static str {
    match disposition {
        Disposition::Set => "set explicitly",
        Disposition::KeepDefault => "keep default",
        Disposition::Omit => "omit",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookforge_core::ir::{
        BlockId, BookFormat, BookId, DomPath, Metadata, Section, SectionId, TextRun,
    };

    fn book_with_text(text: &str) -> Book {
        let section_id = SectionId("section".to_string());
        let block_id = BlockId("block".to_string());
        Book {
            source_path: None,
            id: BookId("book".to_string()),
            format: BookFormat::Epub,
            metadata: Metadata::default(),
            manifest: Vec::new(),
            spine: Vec::new(),
            sections: vec![Section {
                id: section_id.clone(),
                href: "chapter.xhtml".to_string(),
                spine_index: 0,
                title: None,
                heading_level: None,
                block_ids: vec![block_id.clone()],
                prev: None,
                next: None,
            }],
            blocks: vec![Block {
                id: block_id,
                section_id,
                kind: BlockKind::Paragraph,
                dom_path: DomPath(vec![0]),
                text_runs: vec![TextRun {
                    id: "run".to_string(),
                    text: text.to_string(),
                }],
                inline_marks: Vec::new(),
                protected_spans: Vec::new(),
                token_estimate: 1,
            }],
        }
    }

    #[test]
    fn plan_is_deterministic_for_fixed_input() {
        let book = book_with_text(&"The quick brown fox jumps. ".repeat(1_000));
        let first = plan_book(
            &book,
            Path::new("book.epub"),
            Some("Chinese"),
            "Italian",
            "deepseek",
            None,
        )
        .expect("plan should build");
        let second = plan_book(
            &book,
            Path::new("book.epub"),
            Some("Chinese"),
            "Italian",
            "deepseek",
            None,
        )
        .expect("plan should build");
        let wrong_source = plan_book(
            &book,
            Path::new("book.epub"),
            Some("English"),
            "Italian",
            "deepseek",
            None,
        )
        .expect("plan should build");

        assert_eq!(first, second);
        assert_eq!(first.recommendations, wrong_source.recommendations);
        assert_eq!(first.inspection, wrong_source.inspection);
        assert_ne!(
            first.source.declared_language,
            wrong_source.source.declared_language
        );
    }

    #[test]
    fn caseless_source_gets_smaller_bounds_than_similar_cased_source() {
        let cased = book_with_text(&"a".repeat(20_000));
        let caseless = book_with_text(&"矛".repeat(20_000));
        let cased_plan = plan_book(
            &cased,
            Path::new("latin.epub"),
            None,
            "Italian",
            "deepseek",
            None,
        )
        .expect("cased plan should build");
        let caseless_plan = plan_book(
            &caseless,
            Path::new("cjk.epub"),
            Some("English"),
            "Italian",
            "deepseek",
            None,
        )
        .expect("caseless plan should build");

        assert!(
            caseless_plan.recommendations.batch_target_tokens.value
                < cased_plan.recommendations.batch_target_tokens.value
        );
        assert!(
            caseless_plan.recommendations.batch_max_items.value
                < cased_plan.recommendations.batch_max_items.value
        );
        assert_eq!(caseless_plan.source.detected_script, ScriptClass::Caseless);
    }

    #[test]
    fn every_recommendation_has_a_reason() {
        let plan = plan_book(
            &book_with_text("Zarathustra sprach."),
            Path::new("book.epub"),
            None,
            "Italian",
            "openrouter",
            Some("openai/gpt-5.6-luna"),
        )
        .expect("plan should build");

        assert!(plan.recommendations.all_have_reasons());
    }

    #[test]
    fn json_output_is_stable_and_parseable() {
        let plan = plan_book(
            &book_with_text("Zarathustra sprach."),
            Path::new("book.epub"),
            None,
            "Italian",
            "deepseek",
            None,
        )
        .expect("plan should build");
        let first = serde_json::to_string_pretty(&plan).expect("plan should serialize");
        let second = serde_json::to_string_pretty(&plan).expect("plan should serialize");

        assert_eq!(first, second);
        let parsed: serde_json::Value = serde_json::from_str(&first).expect("valid JSON");
        assert_eq!(parsed["schema_version"], PLAN_SCHEMA_VERSION);
        assert_eq!(parsed["advisory_only"], true);
    }

    #[test]
    fn planning_has_no_state_or_provider_side_effects() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let plan = plan_book(
            &book_with_text("Offline text."),
            &scratch.path().join("book.epub"),
            None,
            "Italian",
            "deepseek",
            None,
        )
        .expect("plan should build");

        assert!(plan.advisory_only);
        assert!(!scratch.path().join(".bookforge").exists());
    }

    #[test]
    fn percentile_uses_nearest_rank() {
        let values = (1..=10).collect::<Vec<_>>();
        let distribution = distribution(&values);
        assert_eq!(distribution.median, 5);
        assert_eq!(distribution.p90, 9);
        assert_eq!(distribution.max, 10);
    }
}
