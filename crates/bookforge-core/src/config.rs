use std::path::PathBuf;

use crate::scheduler::SchedulerConfig;

#[derive(Debug, Clone)]
pub struct TranslationConfig {
    pub source_language: Option<String>,
    pub target_language: String,
    pub provider: String,
    pub model: Option<String>,
    pub concurrency: usize,
    pub max_attempts: usize,
    pub output: PathBuf,
}

#[derive(Debug, Clone)]
pub struct SegmentationConfig {
    pub max_segment_tokens: usize,
    pub context_tokens: usize,
}

impl Default for SegmentationConfig {
    fn default() -> Self {
        Self {
            max_segment_tokens: 1_200,
            context_tokens: 160,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PromptVersion {
    V1,
    BatchV1,
    V2,
    BatchV2,
}

impl PromptVersion {
    pub fn as_str(self) -> &'static str {
        match self {
            PromptVersion::V1 => "v1",
            PromptVersion::BatchV1 => "batch_v1",
            PromptVersion::V2 => "v2",
            PromptVersion::BatchV2 => "batch_v2",
        }
    }
}

#[cfg_attr(feature = "cli", derive(clap::ValueEnum))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TranslationProfile {
    Safe,
    Balanced,
    Fastest,
    FreeTier,
    TurboTextOnly,
    V1Fast,
}

impl TranslationProfile {
    pub fn namespace_str(self) -> &'static str {
        match self {
            TranslationProfile::Safe => "safe",
            TranslationProfile::Balanced => "balanced",
            TranslationProfile::Fastest => "fastest",
            TranslationProfile::FreeTier => "free_tier",
            TranslationProfile::TurboTextOnly => "turbo_text_only",
            TranslationProfile::V1Fast => "v1_fast",
        }
    }

    pub fn resolve(self) -> ResolvedRunSettings {
        match self {
            Self::Safe => ResolvedRunSettings {
                profile: self,
                segmentation: SegmentationConfig {
                    max_segment_tokens: 1_200,
                    context_tokens: 160,
                },
                batch: BatchConfig {
                    enabled: false,
                    target_tokens: 0,
                    max_items: 0,
                    adaptive_sizing: false,
                    split_on_json_failure: true,
                    repair_invalid_items: true,
                },
                scheduler: SchedulerConfig {
                    concurrency: 4,
                    max_attempts: 3,
                },
                compact_prompts: false,
                retry_failed_only: false,
                adaptive_concurrency: false,
                provider: ProviderRuntimeConfig {
                    timeout_seconds: 120,
                    provider_max_attempts: 6,
                    validation_max_attempts: 3,
                    retry_after_policy: RetryAfterPolicy::JitteredExponential,
                    max_backoff_seconds: 60,
                    thinking_disabled: false,
                    model_context_tokens: None,
                    max_output_tokens: None,
                    batch_max_output_tokens: None,
                    json_mode: JsonMode::Auto,
                    max_idle_per_host: 32,
                },
                qa: QaRunConfig {
                    concurrency: 4,
                    batch_target_tokens: 4_000,
                    model: None,
                    provider: None,
                    base_url: None,
                    api_key_env: None,
                },
                double_check: DoubleCheckConfig {
                    mode: DoubleCheckMode::Off,
                    model: None,
                    provider: None,
                    base_url: None,
                    api_key_env: None,
                    concurrency: 4,
                    batch_target_tokens: 8_000,
                    auto_correct: false,
                    correction_rounds: 1,
                },
            },
            Self::Balanced => ResolvedRunSettings {
                profile: self,
                segmentation: SegmentationConfig {
                    max_segment_tokens: 2_500,
                    context_tokens: 80,
                },
                batch: BatchConfig {
                    enabled: true,
                    target_tokens: 8_000,
                    max_items: 64,
                    adaptive_sizing: false,
                    split_on_json_failure: true,
                    repair_invalid_items: true,
                },
                scheduler: SchedulerConfig {
                    concurrency: 16,
                    max_attempts: 2,
                },
                compact_prompts: true,
                retry_failed_only: true,
                adaptive_concurrency: true,
                provider: ProviderRuntimeConfig {
                    timeout_seconds: 120,
                    provider_max_attempts: 2,
                    validation_max_attempts: 1,
                    retry_after_policy: RetryAfterPolicy::JitteredExponential,
                    max_backoff_seconds: 30,
                    thinking_disabled: false,
                    model_context_tokens: None,
                    max_output_tokens: None,
                    batch_max_output_tokens: None,
                    json_mode: JsonMode::Auto,
                    max_idle_per_host: 32,
                },
                qa: QaRunConfig {
                    concurrency: 8,
                    batch_target_tokens: 8_000,
                    model: None,
                    provider: None,
                    base_url: None,
                    api_key_env: None,
                },
                double_check: DoubleCheckConfig {
                    mode: DoubleCheckMode::Off,
                    model: None,
                    provider: None,
                    base_url: None,
                    api_key_env: None,
                    concurrency: 4,
                    batch_target_tokens: 8_000,
                    auto_correct: false,
                    correction_rounds: 1,
                },
            },
            Self::Fastest => ResolvedRunSettings {
                profile: self,
                segmentation: SegmentationConfig {
                    max_segment_tokens: 6_000,
                    context_tokens: 20,
                },
                batch: BatchConfig {
                    enabled: true,
                    target_tokens: 16_000,
                    max_items: 160,
                    adaptive_sizing: true,
                    split_on_json_failure: true,
                    repair_invalid_items: true,
                },
                scheduler: SchedulerConfig {
                    concurrency: 64,
                    max_attempts: 1,
                },
                compact_prompts: true,
                retry_failed_only: true,
                adaptive_concurrency: true,
                provider: ProviderRuntimeConfig {
                    timeout_seconds: 120,
                    provider_max_attempts: 2,
                    validation_max_attempts: 1,
                    retry_after_policy: RetryAfterPolicy::JitteredExponential,
                    max_backoff_seconds: 10,
                    thinking_disabled: false,
                    model_context_tokens: None,
                    max_output_tokens: None,
                    batch_max_output_tokens: None,
                    json_mode: JsonMode::Auto,
                    max_idle_per_host: 32,
                },
                qa: QaRunConfig {
                    concurrency: 16,
                    batch_target_tokens: 12_000,
                    model: None,
                    provider: None,
                    base_url: None,
                    api_key_env: None,
                },
                double_check: DoubleCheckConfig {
                    mode: DoubleCheckMode::Off,
                    model: None,
                    provider: None,
                    base_url: None,
                    api_key_env: None,
                    concurrency: 4,
                    batch_target_tokens: 12_000,
                    auto_correct: false,
                    correction_rounds: 1,
                },
            },
            Self::FreeTier => ResolvedRunSettings {
                profile: self,
                segmentation: SegmentationConfig {
                    max_segment_tokens: 2_500,
                    context_tokens: 80,
                },
                batch: BatchConfig {
                    enabled: true,
                    target_tokens: 8_000,
                    max_items: 64,
                    adaptive_sizing: false,
                    split_on_json_failure: false,
                    repair_invalid_items: true,
                },
                scheduler: SchedulerConfig {
                    concurrency: 1,
                    max_attempts: 2,
                },
                compact_prompts: true,
                retry_failed_only: true,
                adaptive_concurrency: true,
                provider: ProviderRuntimeConfig {
                    timeout_seconds: 300,
                    provider_max_attempts: 2,
                    validation_max_attempts: 1,
                    retry_after_policy: RetryAfterPolicy::RespectHeader,
                    max_backoff_seconds: 90,
                    thinking_disabled: false,
                    model_context_tokens: None,
                    max_output_tokens: None,
                    batch_max_output_tokens: None,
                    json_mode: JsonMode::Auto,
                    max_idle_per_host: 8,
                },
                qa: QaRunConfig {
                    concurrency: 1,
                    batch_target_tokens: 4_000,
                    model: None,
                    provider: None,
                    base_url: None,
                    api_key_env: None,
                },
                double_check: DoubleCheckConfig {
                    mode: DoubleCheckMode::Off,
                    model: None,
                    provider: None,
                    base_url: None,
                    api_key_env: None,
                    concurrency: 1,
                    batch_target_tokens: 4_000,
                    auto_correct: false,
                    correction_rounds: 1,
                },
            },
            Self::TurboTextOnly => ResolvedRunSettings {
                profile: self,
                segmentation: SegmentationConfig {
                    max_segment_tokens: 12_000,
                    context_tokens: 0,
                },
                batch: BatchConfig {
                    enabled: true,
                    target_tokens: 24_000,
                    max_items: 250,
                    adaptive_sizing: true,
                    split_on_json_failure: true,
                    repair_invalid_items: false,
                },
                scheduler: SchedulerConfig {
                    concurrency: 96,
                    max_attempts: 1,
                },
                compact_prompts: true,
                retry_failed_only: true,
                adaptive_concurrency: true,
                provider: ProviderRuntimeConfig {
                    timeout_seconds: 120,
                    provider_max_attempts: 1,
                    validation_max_attempts: 1,
                    retry_after_policy: RetryAfterPolicy::None,
                    max_backoff_seconds: 5,
                    thinking_disabled: false,
                    model_context_tokens: None,
                    max_output_tokens: None,
                    batch_max_output_tokens: None,
                    json_mode: JsonMode::Auto,
                    max_idle_per_host: 64,
                },
                qa: QaRunConfig {
                    concurrency: 16,
                    batch_target_tokens: 16_000,
                    model: None,
                    provider: None,
                    base_url: None,
                    api_key_env: None,
                },
                double_check: DoubleCheckConfig {
                    mode: DoubleCheckMode::Off,
                    model: None,
                    provider: None,
                    base_url: None,
                    api_key_env: None,
                    concurrency: 4,
                    batch_target_tokens: 16_000,
                    auto_correct: false,
                    correction_rounds: 1,
                },
            },
            Self::V1Fast => ResolvedRunSettings {
                profile: self,
                segmentation: SegmentationConfig {
                    max_segment_tokens: 12_000,
                    context_tokens: 20,
                },
                batch: BatchConfig {
                    enabled: true,
                    target_tokens: 16_000,
                    max_items: 128,
                    adaptive_sizing: true,
                    split_on_json_failure: true,
                    repair_invalid_items: true,
                },
                scheduler: SchedulerConfig {
                    concurrency: 32,
                    max_attempts: 1,
                },
                compact_prompts: true,
                retry_failed_only: true,
                adaptive_concurrency: true,
                provider: ProviderRuntimeConfig {
                    timeout_seconds: 120,
                    provider_max_attempts: 1,
                    validation_max_attempts: 1,
                    retry_after_policy: RetryAfterPolicy::None,
                    max_backoff_seconds: 5,
                    thinking_disabled: true,
                    model_context_tokens: None,
                    max_output_tokens: None,
                    batch_max_output_tokens: None,
                    json_mode: JsonMode::Auto,
                    max_idle_per_host: 64,
                },
                qa: QaRunConfig {
                    concurrency: 4,
                    batch_target_tokens: 4_000,
                    model: None,
                    provider: None,
                    base_url: None,
                    api_key_env: None,
                },
                double_check: DoubleCheckConfig {
                    mode: DoubleCheckMode::Off,
                    model: None,
                    provider: None,
                    base_url: None,
                    api_key_env: None,
                    concurrency: 4,
                    batch_target_tokens: 8_000,
                    auto_correct: false,
                    correction_rounds: 1,
                },
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedRunSettings {
    pub profile: TranslationProfile,
    pub segmentation: SegmentationConfig,
    pub batch: BatchConfig,
    pub scheduler: SchedulerConfig,
    pub provider: ProviderRuntimeConfig,
    pub compact_prompts: bool,
    pub retry_failed_only: bool,
    pub adaptive_concurrency: bool,
    pub qa: QaRunConfig,
    pub double_check: DoubleCheckConfig,
}

impl ResolvedRunSettings {
    pub fn apply_provider_preset_runtime(&mut self, overrides: ProviderPresetRuntimeOverrides) {
        if let Some(v) = overrides.scheduler_concurrency {
            self.scheduler.concurrency = v.max(1);
        }
        if let Some(v) = overrides.provider_max_attempts {
            self.provider.provider_max_attempts = v.max(1);
        }
        if let Some(v) = overrides.validation_max_attempts {
            self.provider.validation_max_attempts = v.max(1);
        }
        if let Some(v) = overrides.retry_after_policy {
            self.provider.retry_after_policy = v;
        }
        if let Some(v) = overrides.max_backoff_seconds {
            self.provider.max_backoff_seconds = v;
        }
        if let Some(v) = overrides.timeout_seconds {
            self.provider.timeout_seconds = v;
        }
        if let Some(v) = overrides.batch_enabled {
            self.batch.enabled = v;
        }
        if let Some(v) = overrides.batch_target_tokens {
            self.batch.target_tokens = v;
        }
        if let Some(v) = overrides.batch_max_items {
            self.batch.max_items = v;
        }
        if let Some(v) = overrides.adaptive_batch_sizing {
            self.batch.adaptive_sizing = v;
        }
        if let Some(v) = overrides.compact_prompts {
            self.compact_prompts = v;
        }
        if let Some(v) = overrides.adaptive_concurrency {
            self.adaptive_concurrency = v;
        }
        if let Some(v) = overrides.thinking_disabled {
            self.provider.thinking_disabled = v;
        }
        if let Some(v) = overrides.model_context_tokens {
            self.provider.model_context_tokens = Some(v);
        }
        if let Some(v) = overrides.max_output_tokens {
            self.provider.max_output_tokens = Some(v);
        }
        if let Some(v) = overrides.batch_max_output_tokens {
            self.provider.batch_max_output_tokens = Some(v);
        }
        if let Some(v) = overrides.json_mode {
            self.provider.json_mode = v;
        }
        if let Some(v) = overrides.max_idle_per_host {
            self.provider.max_idle_per_host = v;
        }
    }
}

#[derive(Debug, Clone)]
pub struct BatchConfig {
    pub enabled: bool,
    pub target_tokens: usize,
    pub max_items: usize,
    pub adaptive_sizing: bool,
    pub split_on_json_failure: bool,
    pub repair_invalid_items: bool,
}

#[derive(Debug, Clone)]
pub struct QaRunConfig {
    pub concurrency: usize,
    pub batch_target_tokens: usize,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
}

#[cfg_attr(feature = "cli", derive(clap::ValueEnum))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DoubleCheckMode {
    Off,
    Formatting,
    Semantic,
    Full,
}

#[derive(Debug, Clone)]
pub struct DoubleCheckConfig {
    pub mode: DoubleCheckMode,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub concurrency: usize,
    pub batch_target_tokens: usize,
    pub auto_correct: bool,
    pub correction_rounds: usize,
}

#[derive(Debug, Clone)]
pub struct ProviderRuntimeConfig {
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

#[cfg_attr(feature = "cli", derive(clap::ValueEnum))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum JsonMode {
    Auto,
    ResponseFormat,
    PromptOnly,
}

/// Whether sliding-context injection considers all prior segments or only
/// those in the same chapter. Chapter scope keeps concurrency high; book
/// scope serializes across the whole document.
#[cfg_attr(feature = "cli", derive(clap::ValueEnum))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ContextScope {
    #[default]
    Chapter,
    Book,
}

impl ContextScope {
    pub fn as_str(self) -> &'static str {
        match self {
            ContextScope::Chapter => "chapter",
            ContextScope::Book => "book",
        }
    }
}

pub fn cap_output_tokens(
    computed: u32,
    estimated_prompt_tokens: usize,
    model_context_tokens: Option<u32>,
    user_cap: Option<u32>,
) -> u32 {
    let mut out = computed;

    if let Some(context) = model_context_tokens {
        let prompt = estimated_prompt_tokens as u32;
        let remaining = context.saturating_sub(prompt);
        let safe_remaining = remaining.saturating_sub(256);
        out = out.min(safe_remaining.max(512));
    }

    if let Some(cap) = user_cap {
        out = out.min(cap);
    }

    out.max(256)
}

#[cfg_attr(feature = "cli", derive(clap::ValueEnum))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProviderPreset {
    Auto,
    OpenRouterFree,
    OpenRouterPaidFast,
    DeepSeekFree,
    DeepSeekPaid,
    GeminiFlashLite,
    LocalOllama,
    LocalLlamacpp,
    Custom,
}

impl ProviderPreset {
    pub fn resolve(self) -> Option<ProviderPresetResolved> {
        match self {
            ProviderPreset::Auto | ProviderPreset::Custom => None,
            ProviderPreset::OpenRouterFree => Some(ProviderPresetResolved {
                endpoint: ModelEndpoint {
                    provider: "openrouter".to_string(),
                    model: "google/gemini-2.5-flash-lite".to_string(),
                    base_url: Some("https://openrouter.ai/api/v1".to_string()),
                    api_key_env: Some("OPENROUTER_API_KEY".to_string()),
                },
                runtime: ProviderPresetRuntimeOverrides {
                    scheduler_concurrency: Some(2),
                    provider_max_attempts: Some(1),
                    validation_max_attempts: Some(1),
                    retry_after_policy: Some(RetryAfterPolicy::RespectHeader),
                    max_backoff_seconds: Some(90),
                    timeout_seconds: Some(180),
                    batch_enabled: Some(true),
                    batch_target_tokens: Some(6_000),
                    batch_max_items: Some(48),
                    compact_prompts: Some(true),
                    adaptive_concurrency: Some(true),
                    thinking_disabled: Some(true),
                    json_mode: Some(JsonMode::Auto),
                    max_idle_per_host: Some(8),
                    ..Default::default()
                },
            }),
            ProviderPreset::OpenRouterPaidFast => Some(ProviderPresetResolved {
                endpoint: ModelEndpoint {
                    provider: "openrouter".to_string(),
                    model: "google/gemini-2.5-flash".to_string(),
                    base_url: Some("https://openrouter.ai/api/v1".to_string()),
                    api_key_env: Some("OPENROUTER_API_KEY".to_string()),
                },
                runtime: ProviderPresetRuntimeOverrides {
                    scheduler_concurrency: Some(32),
                    provider_max_attempts: Some(1),
                    validation_max_attempts: Some(1),
                    retry_after_policy: Some(RetryAfterPolicy::JitteredExponential),
                    max_backoff_seconds: Some(15),
                    timeout_seconds: Some(120),
                    batch_enabled: Some(true),
                    batch_target_tokens: Some(16_000),
                    batch_max_items: Some(128),
                    adaptive_batch_sizing: Some(true),
                    compact_prompts: Some(true),
                    adaptive_concurrency: Some(true),
                    thinking_disabled: Some(true),
                    json_mode: Some(JsonMode::Auto),
                    max_idle_per_host: Some(64),
                    ..Default::default()
                },
            }),
            ProviderPreset::DeepSeekFree => Some(ProviderPresetResolved {
                endpoint: ModelEndpoint {
                    provider: "deepseek".to_string(),
                    model: "deepseek-v4-flash".to_string(),
                    base_url: Some("https://api.deepseek.com/v1".to_string()),
                    api_key_env: Some("DEEPSEEK_API_KEY".to_string()),
                },
                runtime: ProviderPresetRuntimeOverrides {
                    scheduler_concurrency: Some(1),
                    provider_max_attempts: Some(1),
                    validation_max_attempts: Some(1),
                    retry_after_policy: Some(RetryAfterPolicy::RespectHeader),
                    max_backoff_seconds: Some(120),
                    timeout_seconds: Some(240),
                    batch_enabled: Some(true),
                    batch_target_tokens: Some(4_000),
                    batch_max_items: Some(32),
                    compact_prompts: Some(true),
                    adaptive_concurrency: Some(false),
                    thinking_disabled: Some(true),
                    json_mode: Some(JsonMode::Auto),
                    max_idle_per_host: Some(4),
                    ..Default::default()
                },
            }),
            ProviderPreset::DeepSeekPaid => Some(ProviderPresetResolved {
                endpoint: ModelEndpoint {
                    provider: "deepseek".to_string(),
                    model: "deepseek-v4-flash".to_string(),
                    base_url: Some("https://api.deepseek.com/v1".to_string()),
                    api_key_env: Some("DEEPSEEK_API_KEY".to_string()),
                },
                runtime: ProviderPresetRuntimeOverrides {
                    scheduler_concurrency: Some(8),
                    provider_max_attempts: Some(2),
                    validation_max_attempts: Some(1),
                    retry_after_policy: Some(RetryAfterPolicy::JitteredExponential),
                    max_backoff_seconds: Some(30),
                    timeout_seconds: Some(180),
                    batch_enabled: Some(true),
                    batch_target_tokens: Some(12_000),
                    batch_max_items: Some(96),
                    adaptive_batch_sizing: Some(true),
                    compact_prompts: Some(true),
                    adaptive_concurrency: Some(true),
                    thinking_disabled: Some(true),
                    json_mode: Some(JsonMode::Auto),
                    max_idle_per_host: Some(16),
                    ..Default::default()
                },
            }),
            ProviderPreset::GeminiFlashLite => Some(ProviderPresetResolved {
                endpoint: ModelEndpoint {
                    provider: "openrouter".to_string(),
                    model: "google/gemini-2.5-flash-lite".to_string(),
                    base_url: Some("https://openrouter.ai/api/v1".to_string()),
                    api_key_env: Some("OPENROUTER_API_KEY".to_string()),
                },
                runtime: ProviderPresetRuntimeOverrides {
                    scheduler_concurrency: Some(40),
                    provider_max_attempts: Some(1),
                    validation_max_attempts: Some(1),
                    retry_after_policy: Some(RetryAfterPolicy::JitteredExponential),
                    max_backoff_seconds: Some(15),
                    timeout_seconds: Some(120),
                    batch_enabled: Some(true),
                    batch_target_tokens: Some(20_000),
                    batch_max_items: Some(160),
                    adaptive_batch_sizing: Some(true),
                    compact_prompts: Some(true),
                    adaptive_concurrency: Some(true),
                    thinking_disabled: Some(true),
                    json_mode: Some(JsonMode::Auto),
                    max_idle_per_host: Some(64),
                    ..Default::default()
                },
            }),
            ProviderPreset::LocalOllama => Some(ProviderPresetResolved {
                endpoint: ModelEndpoint {
                    provider: "openai-compatible".to_string(),
                    model: "qwen2.5:14b".to_string(),
                    base_url: Some("http://localhost:11434/v1".to_string()),
                    api_key_env: Some("OLLAMA_API_KEY".to_string()),
                },
                runtime: ProviderPresetRuntimeOverrides {
                    scheduler_concurrency: Some(1),
                    provider_max_attempts: Some(1),
                    validation_max_attempts: Some(1),
                    retry_after_policy: Some(RetryAfterPolicy::None),
                    timeout_seconds: Some(300),
                    batch_enabled: Some(true),
                    batch_target_tokens: Some(4_000),
                    batch_max_items: Some(24),
                    compact_prompts: Some(true),
                    adaptive_concurrency: Some(false),
                    thinking_disabled: Some(true),
                    json_mode: Some(JsonMode::Auto),
                    max_idle_per_host: Some(2),
                    ..Default::default()
                },
            }),
            ProviderPreset::LocalLlamacpp => Some(ProviderPresetResolved {
                endpoint: ModelEndpoint {
                    provider: "openai-compatible".to_string(),
                    model: "local-model".to_string(),
                    base_url: Some("http://localhost:8080/v1".to_string()),
                    api_key_env: Some("LLAMACPP_API_KEY".to_string()),
                },
                runtime: ProviderPresetRuntimeOverrides {
                    scheduler_concurrency: Some(1),
                    provider_max_attempts: Some(1),
                    validation_max_attempts: Some(1),
                    retry_after_policy: Some(RetryAfterPolicy::None),
                    timeout_seconds: Some(300),
                    batch_enabled: Some(true),
                    batch_target_tokens: Some(4_000),
                    batch_max_items: Some(24),
                    compact_prompts: Some(true),
                    adaptive_concurrency: Some(false),
                    thinking_disabled: Some(true),
                    json_mode: Some(JsonMode::Auto),
                    max_idle_per_host: Some(2),
                    ..Default::default()
                },
            }),
        }
    }

    pub fn endpoint_or_default(self, custom: Option<ModelEndpoint>) -> ModelEndpoint {
        if let Some(resolved) = self.resolve() {
            return resolved.endpoint;
        }
        match self {
            ProviderPreset::Auto => ModelEndpoint {
                provider: "deepseek".to_string(),
                model: "deepseek-v4-flash".to_string(),
                base_url: Some("https://api.deepseek.com/v1".to_string()),
                api_key_env: Some("DEEPSEEK_API_KEY".to_string()),
            },
            ProviderPreset::Custom => custom.unwrap_or_else(|| ModelEndpoint {
                provider: "deepseek".to_string(),
                model: "deepseek-v4-flash".to_string(),
                base_url: Some("https://api.deepseek.com/v1".to_string()),
                api_key_env: Some("DEEPSEEK_API_KEY".to_string()),
            }),
            _ => unreachable!("resolved presets returned above"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProviderPresetResolved {
    pub endpoint: ModelEndpoint,
    pub runtime: ProviderPresetRuntimeOverrides,
}

#[derive(Debug, Clone, Default)]
pub struct ProviderPresetRuntimeOverrides {
    pub scheduler_concurrency: Option<usize>,
    pub provider_max_attempts: Option<usize>,
    pub validation_max_attempts: Option<usize>,
    pub retry_after_policy: Option<RetryAfterPolicy>,
    pub max_backoff_seconds: Option<u64>,
    pub timeout_seconds: Option<u64>,
    pub batch_enabled: Option<bool>,
    pub batch_target_tokens: Option<usize>,
    pub batch_max_items: Option<usize>,
    pub adaptive_batch_sizing: Option<bool>,
    pub compact_prompts: Option<bool>,
    pub adaptive_concurrency: Option<bool>,
    pub thinking_disabled: Option<bool>,
    pub model_context_tokens: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub batch_max_output_tokens: Option<u32>,
    pub json_mode: Option<JsonMode>,
    pub max_idle_per_host: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RetryAfterPolicy {
    RespectHeader,
    JitteredExponential,
    Fixed,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProviderErrorKind {
    RateLimit,
    Timeout,
    Server,
    Client,
    InvalidResponse,
    Unknown,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ProviderRequestMetric {
    pub request_id: String,
    pub batch_id: Option<String>,
    pub provider: String,
    pub model: String,
    pub profile: String,
    pub items: usize,
    pub estimated_input_tokens: usize,
    pub max_output_tokens: Option<u32>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub latency_ms: u64,
    pub finish_reason: Option<String>,
    pub status: String,
    pub status_code: Option<u16>,
    pub retry_count: usize,
    pub backoff_ms: u64,
    pub error_kind: Option<ProviderErrorKind>,
}

#[derive(Debug, Clone)]
pub struct ModelEndpoint {
    pub provider: String,
    pub model: String,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ModelRouteConfig {
    pub translation: ModelEndpoint,
    pub repair: Option<ModelEndpoint>,
    pub qa: Option<ModelEndpoint>,
    pub double_check: Option<ModelEndpoint>,
    pub fallback: Option<ModelEndpoint>,
}

#[cfg_attr(feature = "cli", derive(clap::ValueEnum))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FallbackScope {
    Failed,
    NeedsReview,
    FailedAndNeedsReview,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openrouter_paid_fast_preset_sets_runtime_overrides() {
        let resolved = ProviderPreset::OpenRouterPaidFast
            .resolve()
            .expect("preset should resolve");
        assert_eq!(resolved.endpoint.provider, "openrouter");
        assert_eq!(resolved.runtime.scheduler_concurrency, Some(32));
        assert_eq!(resolved.runtime.provider_max_attempts, Some(1));
        assert_eq!(resolved.runtime.batch_target_tokens, Some(16_000));
        assert_eq!(resolved.runtime.adaptive_batch_sizing, Some(true));
        assert_eq!(resolved.runtime.max_idle_per_host, Some(64));
    }

    #[test]
    fn openrouter_free_preset_uses_low_concurrency_and_respect_retry_after() {
        let resolved = ProviderPreset::OpenRouterFree
            .resolve()
            .expect("preset should resolve");
        assert_eq!(resolved.runtime.scheduler_concurrency, Some(2));
        assert_eq!(resolved.runtime.provider_max_attempts, Some(1));
        assert_eq!(
            resolved.runtime.retry_after_policy,
            Some(RetryAfterPolicy::RespectHeader)
        );
        assert_eq!(resolved.runtime.max_idle_per_host, Some(8));
    }

    #[test]
    fn local_presets_use_openai_compatible_loopback_endpoints() {
        let ollama = ProviderPreset::LocalOllama
            .resolve()
            .expect("Ollama preset should resolve");
        assert_eq!(ollama.endpoint.provider, "openai-compatible");
        assert_eq!(
            ollama.endpoint.base_url.as_deref(),
            Some("http://localhost:11434/v1")
        );
        assert_eq!(
            ollama.endpoint.api_key_env.as_deref(),
            Some("OLLAMA_API_KEY")
        );
        assert_eq!(ollama.runtime.scheduler_concurrency, Some(1));

        let llamacpp = ProviderPreset::LocalLlamacpp
            .resolve()
            .expect("llama.cpp preset should resolve");
        assert_eq!(llamacpp.endpoint.provider, "openai-compatible");
        assert_eq!(
            llamacpp.endpoint.base_url.as_deref(),
            Some("http://localhost:8080/v1")
        );
        assert_eq!(
            llamacpp.endpoint.api_key_env.as_deref(),
            Some("LLAMACPP_API_KEY")
        );
    }

    #[test]
    fn deepseek_translation_presets_disable_thinking() {
        for preset in [ProviderPreset::DeepSeekFree, ProviderPreset::DeepSeekPaid] {
            let resolved = preset.resolve().expect("preset should resolve");
            assert_eq!(
                resolved.runtime.thinking_disabled,
                Some(true),
                "translation presets should reserve output tokens for translated prose"
            );
        }
    }

    #[test]
    fn runtime_config_event_includes_provider_preset_values() {
        let event = crate::ProgressEvent::RuntimeConfigResolved {
            profile: "v1_fast".to_string(),
            provider_preset: Some("OpenRouterPaidFast".to_string()),
            provider: "openrouter".to_string(),
            model: "google/gemini-2.5-flash".to_string(),
            concurrency: 32,
            max_attempts: 1,
            provider_max_attempts: 1,
            validation_max_attempts: 1,
            retry_after_policy: "JitteredExponential".to_string(),
            max_backoff_seconds: 15,
            timeout_seconds: 120,
            batch_enabled: true,
            batch_target_tokens: 16_000,
            batch_max_items: 128,
            adaptive_batch_sizing: true,
            adaptive_concurrency: true,
            compact_prompts: true,
            thinking_disabled: true,
            json_mode: "Auto".to_string(),
            model_context_tokens: None,
            max_output_tokens: None,
            batch_max_output_tokens: None,
            timestamp_ms: 0,
        };
        match event {
            crate::ProgressEvent::RuntimeConfigResolved {
                provider_preset,
                batch_target_tokens,
                adaptive_batch_sizing,
                provider_max_attempts,
                ..
            } => {
                assert_eq!(provider_preset.as_deref(), Some("OpenRouterPaidFast"));
                assert_eq!(batch_target_tokens, 16_000);
                assert!(adaptive_batch_sizing);
                assert_eq!(provider_max_attempts, 1);
            }
            _ => unreachable!("constructed runtime event"),
        }
    }

    #[test]
    fn v1_fast_uses_single_provider_attempt() {
        let settings = TranslationProfile::V1Fast.resolve();
        assert_eq!(settings.scheduler.max_attempts, 1);
        assert_eq!(settings.provider.provider_max_attempts, 1);
        assert_eq!(settings.provider.validation_max_attempts, 1);
        assert!(settings.batch.repair_invalid_items);
        assert!(settings.adaptive_concurrency);
        assert!(settings.batch.adaptive_sizing);
    }
}
