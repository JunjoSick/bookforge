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

#[cfg_attr(feature = "cli", derive(clap::ValueEnum))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TranslationProfile {
    Safe,
    Balanced,
    Fastest,
    FreeTier,
    TurboTextOnly,
}

impl TranslationProfile {
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

#[derive(Debug, Clone)]
pub struct BatchConfig {
    pub enabled: bool,
    pub target_tokens: usize,
    pub max_items: usize,
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
