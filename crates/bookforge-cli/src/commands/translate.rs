use anyhow::Result;
use bookforge_core::{
    config::{
        DoubleCheckMode, FallbackScope, ResolvedRunSettings,
        TranslationConfig, TranslationProfile,
    },
    scheduler::SchedulerConfig,
    segment::{BlockTranslation, Segment, SegmentStatus, build_segments, compute_cache_namespace},
};
use bookforge_epub::{read_epub, rebuild_epub};
#[cfg(test)]
use bookforge_llm::translate_segments;
use bookforge_llm::{
    AdaptiveLimiter, LlmError, LlmProvider, MockMode, MockProvider, OpenAiCompatibleConfig,
    OpenAiCompatibleProvider, QaSegmentReview, SegmentTranslation, TelemetryLog,
    TranslationRunConfig, build_translation_batches, qa_segments, run_double_check,
    telemetry_summary, translate_batches_with_callback,
    translate_segments_with_callback,
};
use bookforge_store::{
    CreateJob, JobRecord, JobStore, SaveCachedTranslation, SaveNeedsReview, SaveTranslation,
};
use clap::Args;
use std::path::PathBuf;

use crate::{
    LanguageArgs, ProviderArgs as CliProviderArgs, QaMode,
    cost::estimate_cost_usd,
    default_output_path,
    report::{ReportInput, write_report},
};

#[derive(Debug, Args)]
pub struct TranslateArgs {
    pub input: PathBuf,

    #[command(flatten)]
    pub language: LanguageArgs,

    #[command(flatten)]
    pub provider: CliProviderArgs,

    #[arg(long, value_enum, default_value_t = TranslationProfile::Balanced)]
    pub profile: TranslationProfile,

    #[arg(long)]
    pub max_segment_tokens: Option<usize>,

    #[arg(long)]
    pub context_tokens: Option<usize>,

    #[arg(long)]
    pub batch_target_tokens: Option<usize>,

    #[arg(long)]
    pub batch_max_items: Option<usize>,

    #[arg(long)]
    pub compact_prompts: Option<bool>,

    #[arg(long)]
    pub retry_failed_only: Option<bool>,

    #[arg(long)]
    pub adaptive_concurrency: Option<bool>,

    #[arg(long)]
    pub turbo_text_only: bool,

    #[arg(long, default_value_t = 4)]
    pub concurrency: usize,

    #[arg(long, default_value_t = 3)]
    pub max_attempts: usize,

    #[arg(long)]
    pub provider_max_attempts: Option<usize>,

    #[arg(long)]
    pub validation_max_attempts: Option<usize>,

    #[arg(long)]
    pub out: Option<PathBuf>,

    #[arg(long, value_enum, default_value_t = QaMode::Off)]
    pub qa: QaMode,

    #[arg(long, default_value_t = 8)]
    pub qa_concurrency: usize,

    #[arg(long)]
    pub qa_batch_target_tokens: Option<usize>,

    #[arg(long)]
    pub qa_model: Option<String>,

    #[arg(long)]
    pub qa_provider: Option<String>,

    #[arg(long)]
    pub qa_base_url: Option<String>,

    #[arg(long)]
    pub qa_api_key_env: Option<String>,

    #[arg(long, value_enum, default_value_t = DoubleCheckMode::Off)]
    pub double_check: DoubleCheckMode,

    #[arg(long)]
    pub double_check_model: Option<String>,

    #[arg(long)]
    pub double_check_provider: Option<String>,

    #[arg(long)]
    pub double_check_base_url: Option<String>,

    #[arg(long)]
    pub double_check_api_key_env: Option<String>,

    #[arg(long, default_value_t = 4)]
    pub double_check_concurrency: usize,

    #[arg(long)]
    pub double_check_batch_target_tokens: Option<usize>,

    #[arg(long, default_value_t = false)]
    pub auto_correct: bool,

    #[arg(long, default_value_t = 1)]
    pub correction_rounds: usize,

    #[arg(long)]
    pub fallback_provider: Option<String>,

    #[arg(long)]
    pub fallback_model: Option<String>,

    #[arg(long)]
    pub fallback_base_url: Option<String>,

    #[arg(long)]
    pub fallback_api_key_env: Option<String>,

    #[arg(long, value_enum, default_value_t = FallbackScope::Failed)]
    pub fallback_only: FallbackScope,
}

fn resolve_settings(args: &TranslateArgs) -> ResolvedRunSettings {
    let effective_profile = if args.turbo_text_only
        && !matches!(args.profile, TranslationProfile::TurboTextOnly)
    {
        TranslationProfile::TurboTextOnly
    } else {
        args.profile
    };

    let mut settings = effective_profile.resolve();

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

    settings.scheduler.concurrency = args.concurrency;
    settings.scheduler.max_attempts = args.max_attempts;

    if let Some(v) = args.provider_max_attempts {
        settings.provider.provider_max_attempts = v;
    }
    if let Some(v) = args.validation_max_attempts {
        settings.provider.validation_max_attempts = v;
    }
    settings.provider.timeout_seconds = args.provider.timeout_seconds;

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

    if settings.double_check.mode != DoubleCheckMode::Off
        && settings.double_check.model.is_none()
    {
        eprintln!(
            "--double-check requires --double-check-model unless a default double-check model is configured"
        );
    }

    settings
}

pub async fn run(args: TranslateArgs) -> Result<()> {
    let settings = resolve_settings(&args);
    let output = args
        .out
        .clone()
        .unwrap_or_else(|| default_output_path(&args.input, &args.language.target));
    let config = TranslationConfig {
        source_language: args.language.source.clone(),
        target_language: args.language.target.clone(),
        provider: args.provider.provider.clone(),
        model: args.provider.model.clone(),
        concurrency: settings.scheduler.concurrency,
        max_attempts: settings.scheduler.max_attempts,
        output,
    };

    println!("Input: {}", args.input.display());
    println!("Output: {}", config.output.display());
    println!("Target: {}", config.target_language);
    println!("Provider: {}", config.provider);
    println!("Profile: {:?}", args.profile);
    println!("Concurrency: {}", config.concurrency);
    println!("Batch enabled: {}", settings.batch.enabled);

    if settings.batch.enabled {
        println!("Batch target tokens: {}", settings.batch.target_tokens);
        println!("Batch max items: {}", settings.batch.max_items);
    }

    match config.provider.as_str() {
        "mock" => run_mock_translation(&args.input, &config, &args.provider, &args, &settings).await?,
        "deepseek" | "openrouter" | "openai-compatible" => {
            run_openai_compatible_translation(&args.input, &config, &args.provider, &args, &settings)
                .await?
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

async fn run_mock_translation(
    input: &PathBuf,
    config: &TranslationConfig,
    _provider_args: &CliProviderArgs,
    _cli_args: &TranslateArgs,
    settings: &ResolvedRunSettings,
) -> Result<()> {
    let book = read_epub(input)?;
    let segments = build_segments(&book, &settings.segmentation)?;
    let model = config
        .model
        .clone()
        .unwrap_or_else(|| "mock-prefix-target".to_string());
    let prompt_version = "v1";
    let store = JobStore::open_default()?;
    let job = store.create_job(CreateJob {
        input,
        output: &config.output,
        source_lang: config.source_language.as_deref(),
        target_lang: &config.target_language,
        provider: "mock",
        model: &model,
        base_url: None,
        api_key_env: None,
    })?;
    println!("Job: {}", job.id);
    let cache_namespace = compute_cache_namespace(
        settings.segmentation.max_segment_tokens,
        settings.segmentation.context_tokens,
        &format!("{:?}", settings.profile),
        settings.batch.enabled,
        prompt_version,
    );
    store.insert_segments(&job.id, &segments, prompt_version, "mock", &model, &cache_namespace)?;
    let run_config = TranslationRunConfig {
        source_language: config.source_language.clone(),
        target_language: config.target_language.clone(),
        provider: "mock".to_string(),
        model: model.clone(),
        prompt_version: prompt_version.to_string(),
        temperature: 0.2,
        scheduler: settings.scheduler.clone(),
        profile: settings.profile,
    };
    let provider = MockProvider::new(mock_mode(&model), &config.target_language);
    let mut translations = apply_cached_translations(
        &segments,
        CacheContext {
            store: &store,
            job_id: &job.id,
            prompt_version,
            provider: "mock",
            model: &model,
            source_lang: config.source_language.as_deref(),
            target_lang: &config.target_language,
            cache_namespace: &cache_namespace,
        },
    )?;
    let pending_segments = pending_segments_for_job(&store, &job.id, &segments)?;
    let fresh_translations = translate_and_checkpoint(
        provider.clone(),
        &pending_segments,
        &run_config,
        CheckpointContext {
            store: &store,
            job_id: &job.id,
            provider: "mock",
            model: &model,
            prompt_version,
        },
    )
    .await?;
    translations.extend(fresh_translations);
    translations.sort_by_key(|translation| translation.ordinal);
    let qa_reviews =
        qa_reviews_for_mode(provider, &segments, &translations, &run_config, _cli_args.qa).await;
    mark_job_finished(&store, &job.id, &translations)?;
    print_summary_rebuild_and_report(
        &store,
        &job,
        &book,
        &segments,
        &translations,
        &qa_reviews,
        config,
    )?;

    Ok(())
}

pub(crate) fn mock_mode(model: &str) -> MockMode {
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
    provider_args: &CliProviderArgs,
    cli_args: &TranslateArgs,
    settings: &ResolvedRunSettings,
) -> Result<()> {
    let provider_config = match provider_config(
        &config.provider,
        config.model.as_deref(),
        provider_args.base_url.as_deref(),
        provider_args.api_key_env.as_deref(),
        settings.provider.timeout_seconds,
        settings.provider.provider_max_attempts,
    ) {
        Ok(cfg) => cfg,
        Err(e) => {
            if config.provider == "openai-compatible" {
                return Err(anyhow::anyhow!("--base-url is required for --provider openai-compatible"));
            }
            return Err(e);
        }
    };
    let provider = OpenAiCompatibleProvider::new(provider_config.clone())?;
    let model = provider.model().to_string();
    let book = read_epub(input)?;
    let segments = build_segments(&book, &settings.segmentation)?;
    let prompt_version = "v1";
    let store = JobStore::open_default()?;
    let job = store.create_job(CreateJob {
        input,
        output: &config.output,
        source_lang: config.source_language.as_deref(),
        target_lang: &config.target_language,
        provider: &config.provider,
        model: &model,
        base_url: Some(&provider_config.base_url),
        api_key_env: Some(&provider_config.api_key_env),
    })?;
    println!("Job: {}", job.id);
    let cache_namespace_v1 = compute_cache_namespace(
        settings.segmentation.max_segment_tokens,
        settings.segmentation.context_tokens,
        &format!("{:?}", settings.profile),
        settings.batch.enabled,
        prompt_version,
    );
    store.insert_segments(&job.id, &segments, prompt_version, &config.provider, &model, &cache_namespace_v1)?;
    let run_config = TranslationRunConfig {
        source_language: config.source_language.clone(),
        target_language: config.target_language.clone(),
        provider: config.provider.clone(),
        model: model.clone(),
        prompt_version: prompt_version.to_string(),
        temperature: 0.2,
        scheduler: settings.scheduler.clone(),
        profile: settings.profile,
    };

    if settings.batch.enabled {
        let batch_run_config = TranslationRunConfig {
            source_language: run_config.source_language.clone(),
            target_language: run_config.target_language.clone(),
            provider: run_config.provider.clone(),
            model: run_config.model.clone(),
            prompt_version: "batch_v1".to_string(),
            temperature: run_config.temperature,
            scheduler: SchedulerConfig {
                concurrency: run_config.scheduler.concurrency,
                max_attempts: settings.provider.provider_max_attempts,
            },
            profile: settings.profile,
        };
        let cache_namespace_batch = compute_cache_namespace(
            settings.segmentation.max_segment_tokens,
            settings.segmentation.context_tokens,
            &format!("{:?}", settings.profile),
            settings.batch.enabled,
            "batch_v1",
        );
        store.insert_segments(&job.id, &segments, "batch_v1", &config.provider, &model, &cache_namespace_batch)?;
        let mut translations = apply_cached_translations(
            &segments,
            CacheContext {
                store: &store,
                job_id: &job.id,
                prompt_version: "batch_v1",
                provider: &config.provider,
                model: &model,
                source_lang: config.source_language.as_deref(),
                target_lang: &config.target_language,
                cache_namespace: &cache_namespace_batch,
            },
        )?;
        let pending_segments = pending_segments_for_job(&store, &job.id, &segments)?;
        let fresh_translations = translate_and_checkpoint_batch(
            provider.clone(),
            &pending_segments,
            &batch_run_config,
            settings,
            CheckpointContext {
                store: &store,
                job_id: &job.id,
                provider: &config.provider,
                model: &model,
                prompt_version: "batch_v1",
            },
        )
        .await?;
        translations.extend(fresh_translations);
        translations.sort_by_key(|translation| translation.ordinal);
        let qa_reviews =
            qa_reviews_for_mode(provider.clone(), &segments, &translations, &run_config, cli_args.qa)
                .await;
        translations = run_fallback_pass(
            &provider,
            cli_args,
            &segments,
            translations,
            &store,
            &job.id,
            "batch_v1",
            settings,
        ).await?;
        run_double_check_pass(
            &provider,
            cli_args,
            &segments,
            &translations,
            &run_config,
            settings,
        ).await?;
        mark_job_finished(&store, &job.id, &translations)?;
        print_summary_rebuild_and_report(
            &store,
            &job,
            &book,
            &segments,
            &translations,
            &qa_reviews,
            config,
        )?;
    } else {
        let mut translations = apply_cached_translations(
            &segments,
            CacheContext {
                store: &store,
                job_id: &job.id,
                prompt_version,
                provider: &config.provider,
                model: &model,
                source_lang: config.source_language.as_deref(),
                target_lang: &config.target_language,
                cache_namespace: &cache_namespace_v1,
            },
        )?;
        let pending_segments = pending_segments_for_job(&store, &job.id, &segments)?;
        let fresh_translations = translate_and_checkpoint(
            provider.clone(),
            &pending_segments,
            &run_config,
            CheckpointContext {
                store: &store,
                job_id: &job.id,
                provider: &config.provider,
                model: &model,
                prompt_version,
            },
        )
        .await?;
        translations.extend(fresh_translations);
        translations.sort_by_key(|translation| translation.ordinal);
        let qa_reviews =
            qa_reviews_for_mode(provider.clone(), &segments, &translations, &run_config, cli_args.qa)
                .await;
        translations = run_fallback_pass(
            &provider,
            cli_args,
            &segments,
            translations,
            &store,
            &job.id,
            prompt_version,
            settings,
        ).await?;
        run_double_check_pass(
            &provider,
            cli_args,
            &segments,
            &translations,
            &run_config,
            settings,
        ).await?;
        mark_job_finished(&store, &job.id, &translations)?;
        print_summary_rebuild_and_report(
            &store,
            &job,
            &book,
            &segments,
            &translations,
            &qa_reviews,
            config,
        )?;
    }

    Ok(())
}

fn provider_config(
    provider: &str,
    model: Option<&str>,
    base_url: Option<&str>,
    api_key_env: Option<&str>,
    timeout_seconds: u64,
    provider_max_attempts: usize,
) -> Result<OpenAiCompatibleConfig> {
    let (default_url, default_key_env, default_model) = match provider {
        "deepseek" => ("https://api.deepseek.com/v1", "DEEPSEEK_API_KEY", "deepseek-chat"),
        "openrouter" => ("https://openrouter.ai/api/v1", "OPENROUTER_API_KEY", "openrouter/auto"),
        _ => return Err(anyhow::anyhow!("--base-url is required for --provider {provider}")),
    };

    Ok(OpenAiCompatibleConfig {
        base_url: base_url.map(String::from).unwrap_or_else(|| default_url.to_string()),
        api_key_env: api_key_env.map(String::from).unwrap_or_else(|| default_key_env.to_string()),
        model: model.or(Some(default_model)).map(String::from).unwrap_or_else(|| default_model.to_string()),
        timeout_seconds,
        provider_max_attempts: provider_max_attempts.max(1),
    })
}

async fn translate_and_checkpoint_batch<P>(
    provider: P,
    segments: &[Segment],
    config: &TranslationRunConfig,
    settings: &ResolvedRunSettings,
    checkpoint: CheckpointContext<'_>,
) -> Result<Vec<SegmentTranslation>>
where
    P: LlmProvider,
{
    let batches = build_translation_batches(
        segments,
        &settings.batch,
        settings.profile,
    );

    if batches.is_empty() {
        return translate_and_checkpoint(provider, segments, config, checkpoint).await;
    }

    println!("Batches: {}", batches.len());

    use std::sync::Arc;
    let telemetry = Arc::new(TelemetryLog::new());

    let limiter = if settings.adaptive_concurrency {
        Some(Arc::new(AdaptiveLimiter::new(
            settings.scheduler.concurrency.max(1),
            (settings.scheduler.concurrency * 4).max(1),
        )))
    } else {
        None
    };

    match translate_batches_with_callback(provider, batches, segments, config, telemetry.clone(), limiter, |translation| {
        save_translation_result(
            checkpoint.store,
            checkpoint.job_id,
            translation,
            checkpoint.provider,
            checkpoint.model,
            checkpoint.prompt_version,
        )
        .map_err(|err| LlmError::Provider(format!("checkpoint save failed: {err}")))
    })
    .await
    {
        Ok(translations) => {
            let snapshot = telemetry.snapshot();
            if !snapshot.is_empty() {
                println!("\n{}", telemetry_summary(&snapshot));
            }
            Ok(translations)
        }
        Err(error) => {
            let message = format!(
                "batch translation failed: {error}"
            );
            mark_all_segments_failed(checkpoint.store, checkpoint.job_id, segments, &message)?;
            Err(anyhow::anyhow!(message))
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_fallback_pass(
    primary_provider: &OpenAiCompatibleProvider,
    cli_args: &TranslateArgs,
    segments: &[Segment],
    mut translations: Vec<SegmentTranslation>,
    store: &JobStore,
    job_id: &str,
    prompt_version: &str,
    settings: &ResolvedRunSettings,
) -> Result<Vec<SegmentTranslation>> {
    if cli_args.fallback_provider.is_none() && cli_args.fallback_model.is_none() {
        return Ok(translations);
    }

    let provider_str = cli_args
        .fallback_provider
        .as_deref()
        .unwrap_or("openrouter");
    let model_str = cli_args
        .fallback_model
        .as_deref()
        .unwrap_or(primary_provider.model());

    let fallback_config = provider_config(
        provider_str,
        Some(model_str),
        cli_args.fallback_base_url.as_deref(),
        cli_args.fallback_api_key_env.as_deref(),
        settings.provider.timeout_seconds,
        settings.provider.provider_max_attempts,
    )?;

    let fallback = OpenAiCompatibleProvider::new(fallback_config)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let fallback_model = fallback.model().to_string();

    let candidates: Vec<Segment> = segments
        .iter()
        .filter(|s| {
            let t = translations
                .iter()
                .find(|t| t.segment_id.0 == s.id.0);
            match t {
                Some(t) => match cli_args.fallback_only {
                    FallbackScope::Failed => t.status == SegmentStatus::Failed,
                    FallbackScope::NeedsReview => t.status == SegmentStatus::NeedsReview,
                    FallbackScope::FailedAndNeedsReview => {
                        t.status == SegmentStatus::Failed
                            || t.status == SegmentStatus::NeedsReview
                    }
                },
                None => false,
            }
        })
        .cloned()
        .collect();

    if candidates.is_empty() {
        return Ok(translations);
    }

    println!(
        "Fallback: retrying {} segments with {}/{}",
        candidates.len(),
        provider_str,
        fallback_model
    );

    let run_config = TranslationRunConfig {
        source_language: None,
        target_language: String::new(),
        provider: provider_str.to_string(),
        model: fallback_model.clone(),
        prompt_version: prompt_version.to_string(),
        temperature: 0.2,
        scheduler: SchedulerConfig {
            concurrency: 1,
            max_attempts: settings.provider.provider_max_attempts,
        },
        profile: settings.profile,
    };

    let checkpoint = CheckpointContext {
        store,
        job_id,
        provider: provider_str,
        model: &fallback_model,
        prompt_version,
    };

    let fresh = translate_and_checkpoint(
        fallback,
        &candidates,
        &run_config,
        checkpoint,
    )
    .await?;

    for ft in &fresh {
        if let Some(existing) = translations
            .iter_mut()
            .find(|t| t.segment_id.0 == ft.segment_id.0)
        {
            *existing = ft.clone();
        }
    }

    Ok(translations)
}

async fn run_double_check_pass(
    provider: &OpenAiCompatibleProvider,
    cli_args: &TranslateArgs,
    segments: &[Segment],
    translations: &[SegmentTranslation],
    config: &TranslationRunConfig,
    settings: &ResolvedRunSettings,
) -> Result<()> {
    if settings.double_check.mode == DoubleCheckMode::Off {
        return Ok(());
    }

    let dc_provider = if cli_args.double_check_provider.is_some()
        || cli_args.double_check_model.is_some()
    {
        let provider_str = cli_args.double_check_provider.as_deref().unwrap_or("openrouter");
        let dc_config = provider_config(
            provider_str,
            cli_args.double_check_model.as_deref(),
            cli_args.double_check_base_url.as_deref(),
            cli_args.double_check_api_key_env.as_deref(),
            settings.provider.timeout_seconds,
            settings.provider.provider_max_attempts,
        )?;
        OpenAiCompatibleProvider::new(dc_config)
            .map_err(|e| anyhow::anyhow!("{e}"))?
    } else {
        provider.clone()
    };

    println!("Double-check: auditing translations...");
    let corrections = run_double_check(
        dc_provider,
        segments,
        translations,
        config,
        &settings.double_check,
    )
    .await
    .map_err(|e| anyhow::anyhow!("double-check failed: {e}"))?;

    let applied = corrections
        .iter()
        .filter(|c| matches!(c.status, bookforge_llm::CorrectionStatus::Applied))
        .count();
    let rejected = corrections
        .iter()
        .filter(|c| matches!(c.status, bookforge_llm::CorrectionStatus::RejectedValidationFailed(_)))
        .count();
    let unresolved = corrections
        .iter()
        .filter(|c| matches!(c.status, bookforge_llm::CorrectionStatus::Unresolved))
        .count();

    println!("  Corrections: {applied} applied, {rejected} rejected, {unresolved} unresolved");

    Ok(())
}

#[cfg(test)]
pub(crate) async fn translate_with_scheduler_guard<P>(
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

#[derive(Clone, Copy)]
pub(crate) struct CheckpointContext<'a> {
    pub store: &'a JobStore,
    pub job_id: &'a str,
    pub provider: &'a str,
    pub model: &'a str,
    pub prompt_version: &'a str,
}

#[derive(Clone, Copy)]
pub(crate) struct CacheContext<'a> {
    pub store: &'a JobStore,
    pub job_id: &'a str,
    pub prompt_version: &'a str,
    pub provider: &'a str,
    pub model: &'a str,
    pub source_lang: Option<&'a str>,
    pub target_lang: &'a str,
    pub cache_namespace: &'a str,
}

pub(crate) async fn translate_and_checkpoint<P>(
    provider: P,
    segments: &[Segment],
    config: &TranslationRunConfig,
    checkpoint: CheckpointContext<'_>,
) -> Result<Vec<SegmentTranslation>>
where
    P: LlmProvider,
{
    match translate_segments_with_callback(provider, segments, config, |translation| {
        save_translation_result(
            checkpoint.store,
            checkpoint.job_id,
            translation,
            checkpoint.provider,
            checkpoint.model,
            checkpoint.prompt_version,
        )
        .map_err(|err| LlmError::Provider(format!("checkpoint save failed: {err}")))
    })
    .await
    {
        Ok(translations) => Ok(translations),
        Err(error) => {
            let message = format!(
                "translation scheduler failed before producing per-segment results: {error}"
            );
            mark_all_segments_failed(checkpoint.store, checkpoint.job_id, segments, &message)?;
            Err(anyhow::anyhow!(message))
        }
    }
}

pub(crate) fn apply_cached_translations(
    segments: &[Segment],
    cache: CacheContext<'_>,
) -> Result<Vec<SegmentTranslation>> {
    // Cross prompt-version fallback was removed: namespace + exact
    // block-ID compatibility now gates reuse, so a stale "batch_v1" hit
    // on a "v1" run would be unsafe even with matching block IDs.
    let mut cached = Vec::new();
    for segment in segments {
        let hit = cache.store.find_cached_translation(
            segment,
            cache.prompt_version,
            cache.provider,
            cache.model,
            cache.source_lang,
            cache.target_lang,
            cache.cache_namespace,
        )?;

        let Some(hit) = hit else {
            continue;
        };
        cache.store.save_cached_translation(SaveCachedTranslation {
            job_id: cache.job_id,
            segment_id: &segment.id.0,
            translated_text: &hit.translated_text,
            blocks: &hit.blocks,
            provider: cache.provider,
            model: cache.model,
            prompt_version: cache.prompt_version,
        })?;
        cached.push(SegmentTranslation {
            segment_id: segment.id.clone(),
            ordinal: segment.ordinal,
            block_ids: segment.block_ids.clone(),
            blocks: hit.blocks,
            checksum: segment.checksum.clone(),
            status: SegmentStatus::SkippedCached,
            template: "cached".to_string(),
            error: None,
            input_tokens: None,
            output_tokens: None,
        });
    }
    Ok(cached)
}

pub(crate) fn pending_segments_for_job(
    store: &JobStore,
    job_id: &str,
    segments: &[Segment],
) -> Result<Vec<Segment>> {
    let pending_ids = store.pending_segment_ids(job_id)?;
    let pending = pending_ids
        .iter()
        .map(String::as_str)
        .collect::<std::collections::HashSet<_>>();
    Ok(segments
        .iter()
        .filter(|segment| pending.contains(segment.id.0.as_str()))
        .cloned()
        .collect())
}

pub(crate) async fn qa_reviews_for_mode<P>(
    provider: P,
    segments: &[Segment],
    translations: &[SegmentTranslation],
    config: &TranslationRunConfig,
    qa_mode: QaMode,
) -> Vec<QaSegmentReview>
where
    P: LlmProvider,
{
    match qa_mode {
        QaMode::Off => Vec::new(),
        QaMode::All => qa_segments(provider, segments, translations, config).await,
        QaMode::Suspicious => {
            let candidates = suspicious_qa_candidates(segments, translations);
            qa_segments(provider, segments, &candidates, config).await
        }
    }
}

fn suspicious_qa_candidates(
    segments: &[Segment],
    translations: &[SegmentTranslation],
) -> Vec<SegmentTranslation> {
    let by_segment = segments
        .iter()
        .map(|segment| (segment.id.0.as_str(), segment))
        .collect::<std::collections::HashMap<_, _>>();
    translations
        .iter()
        .filter(|translation| translation.status == SegmentStatus::Succeeded)
        .filter(|translation| {
            let Some(segment) = by_segment.get(translation.segment_id.0.as_str()) else {
                return false;
            };
            let source_len = segment.source.text.chars().count().max(1);
            let translated_len = translation.joined_text().chars().count();
            let ratio = translated_len as f64 / source_len as f64;
            !(0.5..=2.2).contains(&ratio)
                || translation.template == "translate_run_preserving"
                || segment.constraints.preserve_spans.len() >= 4
                || !segment.constraints.preserve_markers.is_empty()
        })
        .cloned()
        .collect()
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

pub(crate) fn save_translation_result(
    store: &JobStore,
    job_id: &str,
    translation: &SegmentTranslation,
    provider: &str,
    model: &str,
    prompt_version: &str,
) -> Result<()> {
    let joined = translation.joined_text();
    match translation.status {
        SegmentStatus::Succeeded => store.save_translation(SaveTranslation {
            job_id,
            segment_id: &translation.segment_id.0,
            translated_text: &joined,
            blocks: &translation.blocks,
            provider,
            model,
            prompt_version,
            input_tokens: translation.input_tokens,
            output_tokens: translation.output_tokens,
        })?,
        SegmentStatus::NeedsReview => store.save_needs_review(SaveNeedsReview {
            job_id,
            segment_id: &translation.segment_id.0,
            preserved_text: &joined,
            blocks: &translation.blocks,
            provider,
            model,
            prompt_version,
            error: translation
                .error
                .as_deref()
                .unwrap_or("translation requires review"),
            input_tokens: translation.input_tokens,
            output_tokens: translation.output_tokens,
        })?,
        SegmentStatus::Failed => store.mark_segment_failed(
            job_id,
            &translation.segment_id.0,
            translation.error.as_deref().unwrap_or("translation failed"),
        )?,
        _ => {}
    }
    Ok(())
}

pub(crate) fn mark_job_finished(
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

pub(crate) fn block_translations(translations: &[SegmentTranslation]) -> Vec<BlockTranslation> {
    translations
        .iter()
        .flat_map(|translation| translation.blocks.iter().cloned())
        .collect()
}

pub(crate) fn print_summary_rebuild_and_report(
    store: &JobStore,
    job: &JobRecord,
    book: &bookforge_core::ir::Book,
    segments: &[Segment],
    translations: &[SegmentTranslation],
    qa_reviews: &[QaSegmentReview],
    config: &TranslationConfig,
) -> Result<()> {
    let block_translations = block_translations(translations);
    rebuild_epub(book, &block_translations, &config.output)?;
    let summary = store
        .summary(&job.id)?
        .ok_or_else(|| anyhow::anyhow!("job '{}' was not found after translation", job.id))?;
    let segment_records = store.segment_records(&job.id)?;
    let report = write_report(ReportInput {
        job,
        summary: &summary,
        segments,
        segment_records: &segment_records,
        translations,
        qa_reviews,
        output: &config.output,
    })?;

    println!(
        "Translated: {}/{} segments",
        summary.succeeded, summary.total_segments
    );
    println!("Cached: {}", summary.cached);
    println!("Retried: {}", summary.retried);
    println!("Needs review: {}", summary.needs_review);
    println!("Failed: {}", summary.failed);
    println!("Input tokens: {}", summary.input_tokens);
    println!("Output tokens: {}", summary.output_tokens);
    if let Some(cost) = estimate_cost_usd(
        &job.provider,
        &job.model,
        summary.input_tokens,
        summary.output_tokens,
    ) {
        println!("Estimated cost: ${cost:.6}");
    }
    println!("Output: {}", config.output.display());
    println!("Report: {}", report.markdown.display());

    Ok(())
}

#[derive(Debug, Args)]
pub struct BenchmarkArgs {
    #[command(flatten)]
    pub provider: CliProviderArgs,

    #[arg(long, default_value_t = 5)]
    pub samples: usize,

    #[arg(long, default_value_t = 1000)]
    pub tokens: usize,

    #[arg(long, default_value_t = 1)]
    pub concurrency: usize,
}

pub async fn run_benchmark(args: BenchmarkArgs) -> Result<()> {
    let pigeon = "Sunt piger, et volare nequeunt. Sed cum cibus apparet, mirabiliter currunt.";
    let provider_config = OpenAiCompatibleConfig {
        base_url: args
            .provider
            .base_url
            .clone()
            .unwrap_or_else(|| "https://openrouter.ai/api/v1".to_string()),
        api_key_env: args
            .provider
            .api_key_env
            .clone()
            .unwrap_or_else(|| "OPENROUTER_API_KEY".to_string()),
        model: args
            .provider
            .model
            .clone()
            .unwrap_or_else(|| "openrouter/auto".to_string()),
        timeout_seconds: args.provider.timeout_seconds,
        provider_max_attempts: 6,
    };

    let provider = OpenAiCompatibleProvider::new(provider_config.clone())?;
    let model = provider.model().to_string();

    println!("Benchmarking {} / {}", provider_config.base_url, model);
    println!("Samples: {}, Tokens: {}, Concurrency: {}", args.samples, args.tokens, args.concurrency);
    println!();

    let mut latencies = Vec::with_capacity(args.samples);
    let mut success_count = 0usize;
    let mut failure_count = 0usize;
    let mut ratelimit_count = 0usize;
    let mut timeout_count = 0usize;
    let mut total_output_tokens = 0u64;
    let mut _total_input_tokens = 0u64;

    for i in 0..args.samples {
        let request = bookforge_llm::CompletionRequest {
            system: "You are a translator. Return JSON only: {\"translation\":\"...\"}".to_string(),
            user: format!(
                "Translate: {{\"text\":\"{}\"}} Return JSON.",
                pigeon
            ),
            response_format: bookforge_llm::ResponseFormat::Json,
            temperature: 0.2,
            max_output_tokens: Some(args.tokens as u32),
            metadata: Default::default(),
        };

        print!("  [{}/{}] ", i + 1, args.samples);
        match provider.complete(request).await {
            Ok(resp) => {
                latencies.push(resp.provider_latency_ms);
                success_count += 1;
                total_output_tokens += resp.output_tokens.unwrap_or(0);
                _total_input_tokens += resp.input_tokens.unwrap_or(0);
                let tok_sec = if resp.provider_latency_ms > 0 {
                    resp.output_tokens.unwrap_or(0) as f64 / (resp.provider_latency_ms as f64 / 1000.0)
                } else {
                    0.0
                };
                println!("OK {}ms finish={:?} in={:?} out={:?} ~{tok_sec:.0}tok/s",
                    resp.provider_latency_ms, resp.finish_reason,
                    resp.input_tokens, resp.output_tokens);
            }
            Err(e) => {
                failure_count += 1;
                let kind = classify_error(&e);
                match kind {
                    "rate_limit" => ratelimit_count += 1,
                    "timeout" => timeout_count += 1,
                    _ => {}
                }
                println!("FAIL [{kind}] {e}");
            }
        }
    }

    println!();
    println!("Results:");
    println!("  Success: {} / {}", success_count, args.samples);
    println!("  Failed:  {}", failure_count);

    if !latencies.is_empty() {
        latencies.sort();
        let p50 = percentile(&latencies, 50);
        let p95 = percentile(&latencies, 95);
        let avg = latencies.iter().sum::<u64>() as f64 / latencies.len() as f64;
        let avg_tok_sec = if avg > 0.0 {
            total_output_tokens as f64 / (avg * latencies.len() as f64 / 1000.0)
        } else {
            0.0
        };

        println!("  p50 latency: {}ms", p50);
        println!("  p95 latency: {}ms", p95);
        println!("  avg latency:  {:.0}ms", avg);
        println!("  avg output:   {:.0} tok/s", avg_tok_sec);
    }

    println!("  429 count:    {}", ratelimit_count);
    println!("  timeout count: {}", timeout_count);

    if !latencies.is_empty() {
        let p50 = percentile(&latencies, 50);
        let recommendation = if ratelimit_count > 0 || p50 > 120_000 {
            ("free-tier", 1usize, 300u64)
        } else if p50 < 15_000 && ratelimit_count == 0 {
            ("fastest", 32usize, 120u64)
        } else {
            ("balanced", 16usize, 120u64)
        };
        println!();
        println!("Recommendation:");
        println!("  profile:     {}", recommendation.0);
        println!("  concurrency: {}", recommendation.1);
        println!("  timeout:     {}s", recommendation.2);
    }

    Ok(())
}

fn classify_error(e: &LlmError) -> &'static str {
    match e {
        LlmError::Http(http_err) => {
            if http_err.is_timeout() {
                "timeout"
            } else {
                "http"
            }
        }
        LlmError::HttpStatus { status, .. } if *status == 429 => "rate_limit",
        LlmError::HttpStatus { status, .. } if (500..600).contains(status) => "server",
        LlmError::HttpStatus { .. } => "client",
        LlmError::Provider(_) => "provider",
        LlmError::InvalidResponse(_) => "invalid_response",
        LlmError::Json(_) => "json",
    }
}

fn percentile(data: &[u64], pct: usize) -> u64 {
    if data.is_empty() {
        return 0;
    }
    let idx = ((pct as f64 / 100.0) * (data.len() - 1) as f64).round() as usize;
    data[idx.min(data.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookforge_core::{
        ir::{BlockId, SectionId},
        segment::{
            SegmentBlock, SegmentConstraints, SegmentContext, SegmentId, SegmentMetadata,
            SegmentSource, SegmentTextRun,
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
            .create_job(CreateJob {
                input: &input_path,
                output: &temp_path("output.epub"),
                source_lang: Some("English"),
                target_lang: "Italian",
                provider: "mock",
                model: "mock-prefix",
                base_url: None,
                api_key_env: None,
            })
            .expect("job should be created");
        let segments = vec![segment("seg_a", 0), segment("seg_b", 1)];
        store
            .insert_segments(&job.id, &segments, "v1", "mock", "mock-prefix", "test_ns")
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
                max_attempts: 1,
            },
            profile: TranslationProfile::Balanced,
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
                    text_runs: vec![SegmentTextRun {
                        id: format!("r{ordinal}"),
                        text: format!("Source {ordinal}"),
                    }],
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

    #[test]
    fn provider_config_sets_provider_max_attempts() {
        let cfg = provider_config("openrouter", None, None, None, 120, 2)
            .expect("provider_config should build");
        assert_eq!(cfg.provider_max_attempts, 2);

        // Zero gets clamped to a minimum of 1.
        let cfg = provider_config("openrouter", None, None, None, 120, 0)
            .expect("provider_config should build");
        assert_eq!(cfg.provider_max_attempts, 1);
    }
}