pub mod config;
pub mod entity;
pub mod error;
pub mod glossary;
pub mod ir;
pub mod marker;
pub mod math;
pub mod progress;
pub mod run_snapshot;
pub mod scheduler;
pub mod segment;
pub mod style;

pub use config::{
    BatchConfig, DoubleCheckConfig, DoubleCheckMode, FallbackScope, JsonMode, ModelEndpoint,
    ModelRouteConfig, PromptVersion, ProviderErrorKind, ProviderPreset, ProviderPresetResolved,
    ProviderPresetRuntimeOverrides, ProviderRequestMetric, ProviderRuntimeConfig, QaRunConfig,
    ResolvedRunSettings, RetryAfterPolicy, SegmentationConfig, TranslationConfig,
    TranslationProfile, cap_output_tokens,
};
pub use entity::{
    Entity, EntityGender, entities_fingerprint, merge_scope_entities, render_entity_agreement_block,
};
pub use error::{BookforgeError, Result};
pub use glossary::{
    GlossaryCandidate, GlossaryCategory, GlossaryFormat, GlossaryPromptTerm, GlossaryScopeKind,
    GlossaryStatus, GlossaryTerm, SegmentGlossarySelections, extract_glossary_candidates,
    merge_scope_terms, select_glossary_for_segments, target_matches, term_matches,
};
pub use progress::{
    IssueEntry, IssueLevel, NullProgressSink, ProgressEvent, ProgressSink, RunState,
    event_timestamp_ms, now_ms,
};
pub use run_snapshot::{ResolvedRunSettingsSnapshot, RunConfigSnapshot};
pub use scheduler::SchedulerConfig;
pub use style::{
    DoNotFields, RegisterFields, StyleSheet, VoiceFields, merge_style_sheets, render_style_block,
    style_fingerprint,
};
