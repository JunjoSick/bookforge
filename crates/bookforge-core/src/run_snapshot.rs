use std::path::PathBuf;

use crate::{
    BatchConfig, BilingualMode, BilingualStyle, DoubleCheckConfig, JsonMode, ProviderPreset,
    ProviderRuntimeConfig, QaRunConfig, ResolvedRunSettings, RetryAfterPolicy, SchedulerConfig,
    SegmentationConfig, TranslationProfile,
    config::ContextScope,
    glossary::{GlossaryFormat, GlossaryTerm},
};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct RunConfigSnapshot {
    pub input_path: PathBuf,
    #[serde(default)]
    pub input_snapshot_path: Option<PathBuf>,
    #[serde(default)]
    pub input_sha256: Option<String>,
    pub output_path: PathBuf,
    pub events_path: Option<PathBuf>,
    pub report_json_path: Option<PathBuf>,
    pub report_markdown_path: Option<PathBuf>,
    pub source_language: Option<String>,
    pub target_language: String,
    pub provider: String,
    pub model: String,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub profile: TranslationProfile,
    pub provider_preset: Option<ProviderPreset>,
    pub prompt_version: String,
    pub cache_namespace: String,
    #[serde(default)]
    pub book_id: Option<String>,
    #[serde(default)]
    pub series_id: Option<String>,
    #[serde(default = "default_glossary_budget_tokens")]
    pub glossary_budget_tokens: usize,
    #[serde(default = "default_glossary_format")]
    pub glossary_format: GlossaryFormat,
    #[serde(default)]
    pub prompt_extra: Option<String>,
    #[serde(default)]
    pub glossary_fingerprint: String,
    #[serde(default)]
    pub glossary_terms: Vec<GlossaryTerm>,
    #[serde(default)]
    pub context_window: usize,
    #[serde(default = "default_context_budget_tokens")]
    pub context_budget_tokens: usize,
    #[serde(default)]
    pub context_scope: ContextScope,
    /// SHA-256 of the merged style sheet's normalized JSON form. Stable
    /// for users without `--style` (fingerprint of `None`).
    #[serde(default)]
    pub style_fingerprint: String,
    /// Pre-rendered style guide block — captured so resume reproduces the
    /// exact prompt the original run sent, even if the source TOML files
    /// have moved or been edited.
    #[serde(default)]
    pub style_rendered_block: String,
    /// SHA-256 of the merged entity set. Same opt-in stance as
    /// `style_fingerprint`: empty rendered block means the cache
    /// namespace ignores this field.
    #[serde(default)]
    pub entities_fingerprint: String,
    /// Pre-rendered entity grammatical-agreement block.
    #[serde(default)]
    pub entities_rendered_block: String,
    #[serde(default)]
    pub bilingual_mode: BilingualMode,
    #[serde(default = "default_bilingual_separator")]
    pub bilingual_separator: String,
    #[serde(default)]
    pub bilingual_style: BilingualStyle,
    #[serde(default)]
    pub bilingual_css: Option<String>,
    #[serde(default)]
    pub fallback: Option<FallbackRunConfigSnapshot>,
    #[serde(default)]
    pub finalize: FinalizeCheckpointSnapshot,
    pub settings: ResolvedRunSettingsSnapshot,
}

fn default_context_budget_tokens() -> usize {
    1200
}

fn default_glossary_budget_tokens() -> usize {
    800
}

fn default_glossary_format() -> GlossaryFormat {
    GlossaryFormat::Json
}

fn default_bilingual_separator() -> String {
    " / ".to_string()
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ResolvedRunSettingsSnapshot {
    pub profile: TranslationProfile,
    pub segmentation: SegmentationConfigSnapshot,
    pub batch: BatchConfigSnapshot,
    pub scheduler: SchedulerConfigSnapshot,
    pub provider: ProviderRuntimeConfigSnapshot,
    pub compact_prompts: bool,
    pub retry_failed_only: bool,
    pub adaptive_concurrency: bool,
    pub qa: QaRunConfigSnapshot,
    pub double_check: DoubleCheckConfigSnapshot,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct FallbackRunConfigSnapshot {
    pub provider: String,
    pub model: String,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub scope: crate::FallbackScope,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct FinalizeCheckpointSnapshot {
    pub double_check_complete: bool,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SegmentationConfigSnapshot {
    pub max_segment_tokens: usize,
    pub context_tokens: usize,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct BatchConfigSnapshot {
    pub enabled: bool,
    pub target_tokens: usize,
    pub max_items: usize,
    pub adaptive_sizing: bool,
    pub split_on_json_failure: bool,
    pub repair_invalid_items: bool,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SchedulerConfigSnapshot {
    pub concurrency: usize,
    pub max_attempts: usize,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ProviderRuntimeConfigSnapshot {
    pub timeout_seconds: u64,
    pub provider_max_attempts: usize,
    pub validation_max_attempts: usize,
    pub retry_after_policy: RetryAfterPolicy,
    pub max_backoff_seconds: u64,
    pub thinking_disabled: bool,
    pub model_context_tokens: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub batch_max_output_tokens: Option<u32>,
    pub json_mode: JsonMode,
    pub max_idle_per_host: usize,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct QaRunConfigSnapshot {
    pub concurrency: usize,
    pub batch_target_tokens: usize,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct DoubleCheckConfigSnapshot {
    pub mode: crate::DoubleCheckMode,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub concurrency: usize,
    pub batch_target_tokens: usize,
    pub auto_correct: bool,
    pub correction_rounds: usize,
}

impl ResolvedRunSettingsSnapshot {
    pub fn from_settings(settings: &ResolvedRunSettings) -> Self {
        Self {
            profile: settings.profile,
            segmentation: SegmentationConfigSnapshot {
                max_segment_tokens: settings.segmentation.max_segment_tokens,
                context_tokens: settings.segmentation.context_tokens,
            },
            batch: BatchConfigSnapshot {
                enabled: settings.batch.enabled,
                target_tokens: settings.batch.target_tokens,
                max_items: settings.batch.max_items,
                adaptive_sizing: settings.batch.adaptive_sizing,
                split_on_json_failure: settings.batch.split_on_json_failure,
                repair_invalid_items: settings.batch.repair_invalid_items,
            },
            scheduler: SchedulerConfigSnapshot {
                concurrency: settings.scheduler.concurrency,
                max_attempts: settings.scheduler.max_attempts,
            },
            provider: ProviderRuntimeConfigSnapshot {
                timeout_seconds: settings.provider.timeout_seconds,
                provider_max_attempts: settings.provider.provider_max_attempts,
                validation_max_attempts: settings.provider.validation_max_attempts,
                retry_after_policy: settings.provider.retry_after_policy,
                max_backoff_seconds: settings.provider.max_backoff_seconds,
                thinking_disabled: settings.provider.thinking_disabled,
                model_context_tokens: settings.provider.model_context_tokens,
                max_output_tokens: settings.provider.max_output_tokens,
                batch_max_output_tokens: settings.provider.batch_max_output_tokens,
                json_mode: settings.provider.json_mode,
                max_idle_per_host: settings.provider.max_idle_per_host,
            },
            compact_prompts: settings.compact_prompts,
            retry_failed_only: settings.retry_failed_only,
            adaptive_concurrency: settings.adaptive_concurrency,
            qa: QaRunConfigSnapshot {
                concurrency: settings.qa.concurrency,
                batch_target_tokens: settings.qa.batch_target_tokens,
                model: settings.qa.model.clone(),
                provider: settings.qa.provider.clone(),
                base_url: settings.qa.base_url.clone(),
                api_key_env: settings.qa.api_key_env.clone(),
            },
            double_check: DoubleCheckConfigSnapshot {
                mode: settings.double_check.mode,
                model: settings.double_check.model.clone(),
                provider: settings.double_check.provider.clone(),
                base_url: settings.double_check.base_url.clone(),
                api_key_env: settings.double_check.api_key_env.clone(),
                concurrency: settings.double_check.concurrency,
                batch_target_tokens: settings.double_check.batch_target_tokens,
                auto_correct: settings.double_check.auto_correct,
                correction_rounds: settings.double_check.correction_rounds,
            },
        }
    }

    pub fn to_settings(&self) -> ResolvedRunSettings {
        ResolvedRunSettings {
            profile: self.profile,
            segmentation: SegmentationConfig {
                max_segment_tokens: self.segmentation.max_segment_tokens,
                context_tokens: self.segmentation.context_tokens,
            },
            batch: BatchConfig {
                enabled: self.batch.enabled,
                target_tokens: self.batch.target_tokens,
                max_items: self.batch.max_items,
                adaptive_sizing: self.batch.adaptive_sizing,
                split_on_json_failure: self.batch.split_on_json_failure,
                repair_invalid_items: self.batch.repair_invalid_items,
            },
            scheduler: SchedulerConfig {
                concurrency: self.scheduler.concurrency,
                max_attempts: self.scheduler.max_attempts,
            },
            provider: ProviderRuntimeConfig {
                timeout_seconds: self.provider.timeout_seconds,
                provider_max_attempts: self.provider.provider_max_attempts,
                validation_max_attempts: self.provider.validation_max_attempts,
                retry_after_policy: self.provider.retry_after_policy,
                max_backoff_seconds: self.provider.max_backoff_seconds,
                thinking_disabled: self.provider.thinking_disabled,
                model_context_tokens: self.provider.model_context_tokens,
                max_output_tokens: self.provider.max_output_tokens,
                batch_max_output_tokens: self.provider.batch_max_output_tokens,
                json_mode: self.provider.json_mode,
                max_idle_per_host: self.provider.max_idle_per_host,
            },
            compact_prompts: self.compact_prompts,
            retry_failed_only: self.retry_failed_only,
            adaptive_concurrency: self.adaptive_concurrency,
            qa: QaRunConfig {
                concurrency: self.qa.concurrency,
                batch_target_tokens: self.qa.batch_target_tokens,
                model: self.qa.model.clone(),
                provider: self.qa.provider.clone(),
                base_url: self.qa.base_url.clone(),
                api_key_env: self.qa.api_key_env.clone(),
            },
            double_check: DoubleCheckConfig {
                mode: self.double_check.mode,
                model: self.double_check.model.clone(),
                provider: self.double_check.provider.clone(),
                base_url: self.double_check.base_url.clone(),
                api_key_env: self.double_check.api_key_env.clone(),
                concurrency: self.double_check.concurrency,
                batch_target_tokens: self.double_check.batch_target_tokens,
                auto_correct: self.double_check.auto_correct,
                correction_rounds: self.double_check.correction_rounds,
            },
        }
    }
}
