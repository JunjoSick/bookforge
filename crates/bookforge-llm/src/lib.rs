pub mod batch;
pub mod concurrency;
pub mod double_check;
pub mod glossary_proposal;
pub mod prompt;
pub mod provider;
pub mod qa_batch;
pub mod rate_controller;
pub mod scheduler;
pub mod telemetry;
mod validation;

pub use batch::{
    BatchItemFailure, BatchItemTranslation, BatchKind, BatchMode, BatchSizer,
    BatchTranslationResult, TranslationBatch, TranslationBatchItem,
    account_for_batch_prompt_overhead, build_translation_batches, collect_repair_items,
    parse_batch_response, split_batch, translate_batches_with_callback,
    translate_batches_with_control,
};
pub use concurrency::{AdaptiveLimiter, PauseSignal, PauseState};
pub use double_check::{
    CorrectionItem, CorrectionRecord, CorrectionStatus, DoubleCheckItem, run_double_check,
};
pub use glossary_proposal::{
    GLOSSARY_PROPOSAL_PROMPT_NAME, GLOSSARY_PROPOSAL_PROMPT_VERSION, GlossaryProposal,
    GlossaryProposalInput, GlossaryProposalPolicy, GlossaryProposalRun,
    propose_glossary_renderings,
};
pub use prompt::{PromptLibrary, PromptTemplate, Rendered, Substitutions};
pub use provider::{
    CompletionRequest, CompletionResponse, FinishReason, LlmError, LlmProvider, MockMode,
    MockProvider, OpenAiCompatibleConfig, OpenAiCompatibleProvider, ProviderCapabilities,
    RequestMetadata, ResponseFormat,
};
pub use qa_batch::{qa_segments_parallel, qa_segments_parallel_with_max_output_tokens};
pub use rate_controller::{
    ProviderRateController, RateControllerConfig, RequestObservation, RequestStatus,
};
pub use scheduler::{
    CompletedContext, ContextRegistry, ContextRunConfig, EngineRuntimeSettings, EntityRunConfig,
    GlossaryRunConfig, QaIssue, QaSegmentReview, SegmentTranslation, StyleRunConfig,
    TranslationRunConfig, qa_segments, translate_segments, translate_segments_with_callback,
    translate_segments_with_control,
};
pub use telemetry::{TelemetryLog, telemetry_summary};
