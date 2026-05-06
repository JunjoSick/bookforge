use bookforge_core::config::{DoubleCheckMode, ResolvedRunSettings, TranslationProfile};

use crate::ProviderArgs as CliProviderArgs;

use super::TranslateArgs;

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
        eprintln!(
            "--double-check requires --double-check-model unless a default double-check model is configured"
        );
    }

    settings
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
