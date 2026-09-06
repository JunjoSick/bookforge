use std::path::PathBuf;

use crate::{
    BatchConfig, BilingualMode, BilingualStyle, DoubleCheckConfig, JsonMode, ProviderPreset,
    ProviderRuntimeConfig, QaRunConfig, ResolvedRunSettings, RetryAfterPolicy, SchedulerConfig,
    SegmentationConfig, TranslationProfile,
    config::ContextScope,
    glossary::{GlossaryFormat, GlossaryTerm},
    segment::{CacheIdentity, Segment},
};

/// The actual per-segment prompt ingredients that shape the rendered request.
///
/// The cache identity hashes these strings (not just the configuration that
/// produced them): two runs whose config fingerprints agree but whose rendered
/// content differs — different neighbor text, a different per-segment glossary
/// selection, an edited style/entity block — must never reuse a cache row.
/// [`RunConfigSnapshot::cache_identity`] combines them with the segment's own
/// neighbor context and the provider/runtime settings into one fingerprint.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CachePromptInputs {
    /// Canonical rendering of the actual per-segment glossary terms selected
    /// for this segment (ordered, budget-bounded; empty when no glossary).
    pub glossary_rendered: String,
    /// The actual rendered style-guide block substituted into the prompt.
    pub style_rendered: String,
    /// The actual rendered entity-agreement block substituted into the prompt.
    pub entities_rendered: String,
}

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
    #[serde(default)]
    pub creator: Option<String>,
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
    /// CLI QA scope captured outside `ResolvedRunSettings` (off, suspicious,
    /// or all). Kept as a string so the core snapshot does not depend on the
    /// CLI's clap enum.
    #[serde(default = "default_qa_mode")]
    pub qa_mode: String,
    #[serde(default)]
    pub validate_output: bool,
    pub settings: ResolvedRunSettingsSnapshot,
}

fn default_qa_mode() -> String {
    "off".to_string()
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

/// Durable cache-policy fields that are not part of [`RunConfigSnapshot`]'s
/// historical surface and therefore live in their own persisted record
/// (`jobs.cache_policy_json`). Old jobs that never recorded a policy read
/// back the conservative default, which prevents their cache entries from
/// being reused by runs that state an explicit policy.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct CachePolicySnapshot {
    /// The strict-context completion fence (`ContextRunConfig::strict`).
    /// `None` means the run never recorded its choice — hashed distinctly
    /// from both explicit values so legacy cache rows can never be reused by
    /// a run that pins strictness (and vice versa).
    #[serde(default)]
    pub strict_context: Option<bool>,
}

impl CachePolicySnapshot {
    /// Conservative fallback for rows that predate the policy record: no
    /// explicit strictness. Fingerprints built with this value are
    /// incompatible with any run that states `Some(true)` or `Some(false)`.
    pub fn conservative() -> Self {
        Self {
            strict_context: None,
        }
    }
}

impl RunConfigSnapshot {
    /// Build the structured [`CacheIdentity`] for one segment from this
    /// snapshot's full output-affecting settings plus the ACTUAL rendered
    /// prompt ingredients (`request.prompt_inputs`) and the segment's own
    /// neighbor context. `request.strict_context` comes from the separately
    /// persisted [`CachePolicySnapshot`]; pass `None` when the job never
    /// recorded a policy (conservative).
    pub fn cache_identity(&self, request: CacheIdentityRequest<'_>) -> CacheIdentity {
        let CacheIdentityRequest {
            segment,
            provider,
            model,
            prompt_version,
            cache_namespace,
            strict_context,
            prompt_inputs,
        } = request;
        CacheIdentity {
            schema_version: crate::segment::CACHE_IDENTITY_SCHEMA_VERSION,
            source_hash: segment.checksum.clone(),
            provider: provider.to_string(),
            model: model.to_string(),
            source_lang: self.source_language.clone(),
            target_lang: self.target_language.clone(),
            prompt_version: prompt_version.to_string(),
            cache_namespace: cache_namespace.to_string(),
            prompt_extra: self.prompt_extra.clone(),
            max_segment_tokens: self.settings.segmentation.max_segment_tokens,
            context_tokens: self.settings.segmentation.context_tokens,
            context_window: self.context_window,
            context_budget_tokens: self.context_budget_tokens,
            context_scope: self.context_scope,
            strict_context,
            profile: self.profile,
            batch_enabled: self.settings.batch.enabled,
            batch_target_tokens: self.settings.batch.target_tokens,
            batch_max_items: self.settings.batch.max_items,
            batch_adaptive_sizing: self.settings.batch.adaptive_sizing,
            batch_split_on_json_failure: self.settings.batch.split_on_json_failure,
            batch_repair_invalid_items: self.settings.batch.repair_invalid_items,
            compact_prompts: self.settings.compact_prompts,
            glossary_fingerprint: self.glossary_fingerprint.clone(),
            style_fingerprint: self.style_fingerprint.clone(),
            entities_fingerprint: self.entities_fingerprint.clone(),
            context_before: segment.context.before.clone().unwrap_or_default(),
            context_after: segment.context.after.clone().unwrap_or_default(),
            glossary_rendered: prompt_inputs.glossary_rendered.clone(),
            style_rendered: prompt_inputs.style_rendered.clone(),
            entities_rendered: prompt_inputs.entities_rendered.clone(),
            bilingual_mode: self.bilingual_mode,
            bilingual_separator: self.bilingual_separator.clone(),
            bilingual_style: self.bilingual_style,
            thinking_disabled: self.settings.provider.thinking_disabled,
            max_output_tokens: self.settings.provider.max_output_tokens,
            batch_max_output_tokens: self.settings.provider.batch_max_output_tokens,
            json_mode: self.settings.provider.json_mode,
        }
    }
}

/// Inputs for [`RunConfigSnapshot::cache_identity`]: the request-visible
/// fields plus the actual rendered prompt ingredients. Bundling them keeps the
/// identity construction signature stable as more request fields are added.
#[derive(Debug, Clone, Copy)]
pub struct CacheIdentityRequest<'a> {
    pub segment: &'a Segment,
    pub provider: &'a str,
    pub model: &'a str,
    pub prompt_version: &'a str,
    pub cache_namespace: &'a str,
    pub strict_context: Option<bool>,
    pub prompt_inputs: &'a CachePromptInputs,
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
    /// The offline plan applied when this job was created. This stays beside
    /// the other durable run-evolution metadata so later reconfiguration can
    /// replace resolved settings without erasing the original rationale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_plan: Option<AppliedPlanSnapshot>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct AppliedPlanSnapshot {
    pub schema_version: u32,
    pub decisions: Vec<AppliedPlanDecisionSnapshot>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct AppliedPlanDecisionSnapshot {
    pub setting: String,
    pub value: serde_json::Value,
    pub reason: String,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_policy_conservative_is_unknown_strictness() {
        assert_eq!(CachePolicySnapshot::conservative().strict_context, None);
        let serialized =
            serde_json::to_string(&CachePolicySnapshot::conservative()).expect("policy serializes");
        let round_trip: CachePolicySnapshot =
            serde_json::from_str(&serialized).expect("policy deserializes");
        assert_eq!(round_trip, CachePolicySnapshot::conservative());
    }

    #[test]
    fn cache_policy_missing_field_deserializes_to_conservative_default() {
        let empty = CachePolicySnapshot::conservative();
        let from_partial: CachePolicySnapshot = serde_json::from_str("{}").expect("empty policy");
        assert_eq!(from_partial, empty);
        assert_eq!(from_partial.strict_context, None);
    }

    #[test]
    fn cache_identity_carries_snapshot_settings_and_strict_context() {
        let mut snapshot = RunConfigSnapshot {
            input_path: PathBuf::from("input.epub"),
            input_snapshot_path: None,
            input_sha256: None,
            output_path: PathBuf::from("output.epub"),
            events_path: None,
            report_json_path: None,
            report_markdown_path: None,
            source_language: Some("English".to_string()),
            target_language: "Italian".to_string(),
            creator: None,
            provider: "openrouter".to_string(),
            model: "google/gemini-2.5-flash".to_string(),
            base_url: None,
            api_key_env: None,
            profile: TranslationProfile::Balanced,
            provider_preset: None,
            prompt_version: "batch_v3".to_string(),
            cache_namespace: "legacy_namespace".to_string(),
            book_id: None,
            series_id: None,
            glossary_budget_tokens: 800,
            glossary_format: GlossaryFormat::Json,
            prompt_extra: None,
            glossary_fingerprint: "glossary:a".to_string(),
            glossary_terms: Vec::new(),
            context_window: 4,
            context_budget_tokens: 400,
            context_scope: ContextScope::Chapter,
            style_fingerprint: String::new(),
            style_rendered_block: String::new(),
            entities_fingerprint: String::new(),
            entities_rendered_block: String::new(),
            bilingual_mode: BilingualMode::Replace,
            bilingual_separator: " / ".to_string(),
            bilingual_style: BilingualStyle::Minimal,
            bilingual_css: None,
            fallback: None,
            finalize: FinalizeCheckpointSnapshot::default(),
            qa_mode: "off".to_string(),
            validate_output: false,
            settings: ResolvedRunSettingsSnapshot {
                profile: TranslationProfile::Balanced,
                segmentation: SegmentationConfigSnapshot {
                    max_segment_tokens: 2_500,
                    context_tokens: 80,
                },
                batch: BatchConfigSnapshot {
                    enabled: true,
                    target_tokens: 8_000,
                    max_items: 64,
                    adaptive_sizing: false,
                    split_on_json_failure: true,
                    repair_invalid_items: true,
                },
                scheduler: SchedulerConfigSnapshot {
                    concurrency: 16,
                    max_attempts: 2,
                },
                provider: ProviderRuntimeConfigSnapshot {
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
                compact_prompts: true,
                retry_failed_only: true,
                adaptive_concurrency: true,
                qa: QaRunConfigSnapshot {
                    concurrency: 8,
                    batch_target_tokens: 8_000,
                    model: None,
                    provider: None,
                    base_url: None,
                    api_key_env: None,
                },
                double_check: DoubleCheckConfigSnapshot {
                    mode: crate::DoubleCheckMode::Off,
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
        };
        snapshot.settings.segmentation.max_segment_tokens = 2_500;

        let segment = crate::segment::Segment {
            id: crate::segment::SegmentId("seg_a".to_string()),
            section_id: crate::ir::SectionId("sec_0".to_string()),
            ordinal: 0,
            block_ids: Vec::new(),
            source: crate::segment::SegmentSource {
                text: "source".to_string(),
                blocks: Vec::new(),
                token_estimate: 2,
            },
            context: crate::segment::SegmentContext::default(),
            metadata: crate::segment::SegmentMetadata::default(),
            constraints: crate::segment::SegmentConstraints::default(),
            checksum: "checksum_a".to_string(),
        };

        let default_inputs = CachePromptInputs::default();
        let request = |strict_context| CacheIdentityRequest {
            segment: &segment,
            provider: "openrouter",
            model: "google/gemini-2.5-flash",
            prompt_version: "batch_v3",
            cache_namespace: "legacy_namespace",
            strict_context,
            prompt_inputs: &default_inputs,
        };

        let loose = snapshot.cache_identity(request(Some(false)));
        let strict = snapshot.cache_identity(request(Some(true)));
        let unknown = snapshot.cache_identity(request(None));

        assert_ne!(loose.fingerprint(), strict.fingerprint());
        assert_ne!(unknown.fingerprint(), loose.fingerprint());
        assert_ne!(unknown.fingerprint(), strict.fingerprint());
        assert_eq!(snapshot.settings.segmentation.max_segment_tokens, 2_500);
        assert_eq!(
            snapshot.cache_identity(request(Some(false))),
            loose,
            "identity construction is deterministic"
        );
    }

    #[test]
    fn cache_identity_hashes_actual_rendered_prompt_inputs() {
        let snapshot = RunConfigSnapshot {
            input_path: PathBuf::from("input.epub"),
            input_snapshot_path: None,
            input_sha256: None,
            output_path: PathBuf::from("output.epub"),
            events_path: None,
            report_json_path: None,
            report_markdown_path: None,
            source_language: Some("English".to_string()),
            target_language: "Italian".to_string(),
            creator: None,
            provider: "openrouter".to_string(),
            model: "google/gemini-2.5-flash".to_string(),
            base_url: None,
            api_key_env: None,
            profile: TranslationProfile::Balanced,
            provider_preset: None,
            prompt_version: "batch_v3".to_string(),
            cache_namespace: "legacy_namespace".to_string(),
            book_id: None,
            series_id: None,
            glossary_budget_tokens: 800,
            glossary_format: GlossaryFormat::Json,
            prompt_extra: None,
            glossary_fingerprint: "same_config_fp".to_string(),
            glossary_terms: Vec::new(),
            context_window: 4,
            context_budget_tokens: 400,
            context_scope: ContextScope::Chapter,
            style_fingerprint: String::new(),
            style_rendered_block: String::new(),
            entities_fingerprint: String::new(),
            entities_rendered_block: String::new(),
            bilingual_mode: BilingualMode::Replace,
            bilingual_separator: " / ".to_string(),
            bilingual_style: BilingualStyle::Minimal,
            bilingual_css: None,
            fallback: None,
            finalize: FinalizeCheckpointSnapshot::default(),
            qa_mode: "off".to_string(),
            validate_output: false,
            settings: snapshot_fixture_settings(),
        };

        let segment = crate::segment::Segment {
            id: crate::segment::SegmentId("seg_a".to_string()),
            section_id: crate::ir::SectionId("sec_0".to_string()),
            ordinal: 0,
            block_ids: Vec::new(),
            source: crate::segment::SegmentSource {
                text: "identical text".to_string(),
                blocks: Vec::new(),
                token_estimate: 2,
            },
            context: crate::segment::SegmentContext {
                before: Some("before A".to_string()),
                after: Some("after A".to_string()),
            },
            metadata: crate::segment::SegmentMetadata::default(),
            constraints: crate::segment::SegmentConstraints::default(),
            checksum: "checksum_identical".to_string(),
        };
        let mut segment_b = segment.clone();
        segment_b.context.before = Some("before B".to_string());
        segment_b.context.after = Some("after B".to_string());

        let inputs_a = CachePromptInputs {
            glossary_rendered: "[{\"source\":\"hello\",\"target\":\"ciao\"}]".to_string(),
            style_rendered: "Style block A".to_string(),
            entities_rendered: "Entity block A".to_string(),
        };
        let inputs_b = CachePromptInputs {
            glossary_rendered: "[{\"source\":\"world\",\"target\":\"mondo\"}]".to_string(),
            style_rendered: "Style block B".to_string(),
            entities_rendered: "Entity block B".to_string(),
        };

        let identity = |segment: &crate::segment::Segment, inputs: &CachePromptInputs| {
            snapshot.cache_identity(CacheIdentityRequest {
                segment,
                provider: "openrouter",
                model: "google/gemini-2.5-flash",
                prompt_version: "batch_v3",
                cache_namespace: "legacy_namespace",
                strict_context: Some(false),
                prompt_inputs: inputs,
            })
        };

        let a = identity(&segment, &inputs_a);
        let b = identity(&segment_b, &inputs_b);
        assert_eq!(a.source_hash, b.source_hash);
        assert_ne!(a.fingerprint(), b.fingerprint());

        // Identical config fingerprint but different actual rendered blocks
        // must still differ (same snapshot, different prompt inputs).
        let rendered_difference = identity(&segment, &inputs_b);
        assert_ne!(a.fingerprint(), rendered_difference.fingerprint());
    }

    fn snapshot_fixture_settings() -> ResolvedRunSettingsSnapshot {
        let settings = crate::TranslationProfile::Balanced.resolve();
        ResolvedRunSettingsSnapshot::from_settings(&settings)
    }
}
