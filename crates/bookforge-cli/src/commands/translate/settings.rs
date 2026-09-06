use bookforge_core::{
    config::{DoubleCheckMode, ResolvedRunSettings, TranslationProfile},
    run_snapshot::{AppliedPlanDecisionSnapshot, AppliedPlanSnapshot},
};

use crate::ProviderArgs as CliProviderArgs;

use super::TranslateArgs;
use crate::commands::plan::{Disposition, Plan};

pub fn apply_provider_preset(
    explicit: &CliProviderArgs,
    preset: Option<bookforge_core::ProviderPreset>,
) -> CliProviderArgs {
    let Some(p) = preset else {
        return explicit.clone();
    };
    let endpoint = p.endpoint_or_default(None);
    CliProviderArgs {
        provider: endpoint.provider,
        model: explicit.model.clone().or(Some(endpoint.model)),
        base_url: explicit.base_url.clone().or(endpoint.base_url),
        api_key_env: explicit.api_key_env.clone().or(endpoint.api_key_env),
        timeout_seconds: explicit.timeout_seconds,
    }
}

pub fn resolve_settings(args: &TranslateArgs) -> ResolvedRunSettings {
    let effective_profile =
        if args.turbo_text_only && !matches!(args.profile, TranslationProfile::TurboTextOnly) {
            TranslationProfile::TurboTextOnly
        } else {
            args.profile
        };

    let mut settings = effective_profile.resolve();

    if let Some(preset) = args.provider_preset.and_then(|preset| preset.resolve()) {
        settings.apply_provider_preset_runtime(preset.runtime);
    }

    // Built-in target styles can carry conservative planning defaults. Toki
    // Pona commonly expands dense source prose several-fold, so the ordinary
    // v1-fast 12k/16k units would predictably truncate before adaptive
    // splitting converged. Apply the policy before explicit CLI overrides.
    if let Some(policy) =
        bookforge_core::style::built_in_sizing_policy_for_target(&args.language.target)
    {
        settings.segmentation.max_segment_tokens = settings
            .segmentation
            .max_segment_tokens
            .min(policy.max_segment_tokens);
        settings.batch.target_tokens = settings.batch.target_tokens.min(policy.batch_target_tokens);
        settings.batch.max_items = settings.batch.max_items.min(policy.batch_max_items);
        settings.batch.adaptive_sizing = policy.adaptive_sizing;
    }

    if let Some(v) = args.max_segment_tokens {
        settings.segmentation.max_segment_tokens = v;
    }
    if let Some(v) = args.context_tokens {
        settings.segmentation.context_tokens = v;
    }
    if let Some(v) = args.batch_target_tokens {
        settings.batch.target_tokens = v;
    }
    if let Some(v) = args.batch_max_items {
        settings.batch.max_items = v;
    }
    if let Some(v) = args.compact_prompts {
        settings.compact_prompts = v;
    }
    if let Some(v) = args.retry_failed_only {
        settings.retry_failed_only = v;
    }
    if let Some(v) = args.adaptive_concurrency {
        settings.adaptive_concurrency = v;
    }

    if let Some(v) = args.concurrency {
        settings.scheduler.concurrency = v;
    }
    if let Some(v) = args.max_attempts {
        settings.scheduler.max_attempts = v;
    }

    if let Some(v) = args.provider_max_attempts {
        settings.provider.provider_max_attempts = v;
    }
    if let Some(v) = args.validation_max_attempts {
        settings.provider.validation_max_attempts = v;
    }
    if let Some(v) = args.provider.timeout_seconds {
        settings.provider.timeout_seconds = v;
    }
    if args.no_thinking {
        settings.provider.thinking_disabled = true;
    }
    if let Some(v) = args.model_context_tokens {
        settings.provider.model_context_tokens = Some(v);
    }
    if let Some(v) = args.max_output_tokens {
        settings.provider.max_output_tokens = Some(v);
    }
    if let Some(v) = args.batch_max_output_tokens {
        settings.provider.batch_max_output_tokens = Some(v);
    }
    settings.provider.json_mode = args.json_mode;

    settings.qa.concurrency = args.qa_concurrency;
    if let Some(v) = args.qa_batch_target_tokens {
        settings.qa.batch_target_tokens = v;
    }
    settings.qa.model = args.qa_model.clone();
    settings.qa.provider = args.qa_provider.clone();
    settings.qa.base_url = args.qa_base_url.clone();
    settings.qa.api_key_env = args.qa_api_key_env.clone();

    settings.double_check.mode = args.double_check;
    settings.double_check.model = args.double_check_model.clone();
    settings.double_check.provider = args.double_check_provider.clone();
    settings.double_check.base_url = args.double_check_base_url.clone();
    settings.double_check.api_key_env = args.double_check_api_key_env.clone();
    settings.double_check.concurrency = args.double_check_concurrency;
    if let Some(v) = args.double_check_batch_target_tokens {
        settings.double_check.batch_target_tokens = v;
    }
    settings.double_check.auto_correct = args.auto_correct;
    settings.double_check.correction_rounds = args.correction_rounds;

    if settings.double_check.mode != DoubleCheckMode::Off && settings.double_check.model.is_none() {
        // Be honest about what actually happens: without an explicit
        // --double-check-model the audit runs against the primary provider and
        // model, it is not refused (CLI-12).
        eprintln!(
            "warning: --double-check is enabled without --double-check-model; the audit will reuse \
             the primary translation provider/model. Configure --double-check-provider/--double-check-model \
             to review with a different model."
        );
    }

    settings
}

/// Apply the actionable parts of an offline plan after profile, target-policy,
/// and provider-preset defaults have resolved. Direct setting flags are tested
/// field by field and always retain precedence.
pub fn apply_plan_recommendations(
    args: &TranslateArgs,
    settings: &mut ResolvedRunSettings,
    plan: &Plan,
) -> AppliedPlanSnapshot {
    let recommendations = &plan.recommendations;
    let mut decisions = Vec::new();

    if args.batch_target_tokens.is_none()
        && recommendations.batch_target_tokens.disposition == Disposition::Set
        && settings.batch.target_tokens != recommendations.batch_target_tokens.value
    {
        settings.batch.target_tokens = recommendations.batch_target_tokens.value;
        push_plan_decision(
            &mut decisions,
            "batch_target_tokens",
            recommendations.batch_target_tokens.value,
            &recommendations.batch_target_tokens.reason,
        );
    }
    if args.batch_max_items.is_none()
        && recommendations.batch_max_items.disposition == Disposition::Set
        && settings.batch.max_items != recommendations.batch_max_items.value
    {
        settings.batch.max_items = recommendations.batch_max_items.value;
        push_plan_decision(
            &mut decisions,
            "batch_max_items",
            recommendations.batch_max_items.value,
            &recommendations.batch_max_items.reason,
        );
    }
    if args.batch_max_output_tokens.is_none()
        && let Some(value) = recommendations.batch_max_output_tokens.value
        && settings.provider.batch_max_output_tokens != Some(value)
    {
        settings.provider.batch_max_output_tokens = Some(value);
        push_plan_decision(
            &mut decisions,
            "batch_max_output_tokens",
            value,
            &recommendations.batch_max_output_tokens.reason,
        );
    }
    if args.max_output_tokens.is_none()
        && settings.provider.max_output_tokens != Some(recommendations.max_output_tokens.value)
    {
        settings.provider.max_output_tokens = Some(recommendations.max_output_tokens.value);
        push_plan_decision(
            &mut decisions,
            "max_output_tokens",
            recommendations.max_output_tokens.value,
            &recommendations.max_output_tokens.reason,
        );
    }
    if !args.no_thinking
        && recommendations.no_thinking.value
        && !settings.provider.thinking_disabled
    {
        settings.provider.thinking_disabled = true;
        push_plan_decision(
            &mut decisions,
            "thinking_disabled",
            true,
            &recommendations.no_thinking.reason,
        );
    }

    AppliedPlanSnapshot {
        schema_version: plan.schema_version,
        decisions,
    }
}

fn push_plan_decision<T: serde::Serialize>(
    decisions: &mut Vec<AppliedPlanDecisionSnapshot>,
    setting: &str,
    value: T,
    reason: &str,
) {
    decisions.push(AppliedPlanDecisionSnapshot {
        setting: setting.to_string(),
        value: serde_json::to_value(value).expect("plan setting values always serialize"),
        reason: reason.to_string(),
    });
}

pub fn retry_amplification_warning(settings: &ResolvedRunSettings) -> Option<String> {
    let scheduler_provider_product =
        settings.scheduler.max_attempts * settings.provider.provider_max_attempts;
    if scheduler_provider_product < 6 {
        return None;
    }
    let total = scheduler_provider_product * settings.provider.validation_max_attempts;
    Some(format!(
        "scheduler attempts {} x provider attempts {} can produce up to {} calls per failed unit before validation retries ({} total with validation attempts {})",
        settings.scheduler.max_attempts,
        settings.provider.provider_max_attempts,
        scheduler_provider_product,
        total,
        settings.provider.validation_max_attempts
    ))
}
