pub mod batch;
pub mod concurrency;
pub mod double_check;
pub mod prompt;
pub mod provider;
pub mod qa_batch;
pub mod rate_controller;
pub mod scheduler;
pub mod telemetry;

pub use batch::{
    BatchItemFailure, BatchItemTranslation, BatchKind, BatchMode, BatchSizer,
    BatchTranslationResult, TranslationBatch, TranslationBatchItem,
    account_for_batch_prompt_overhead, build_translation_batches, collect_repair_items,
    parse_batch_response, split_batch, translate_batches_with_callback,
};
pub use concurrency::AdaptiveLimiter;
pub use double_check::{
    CorrectionItem, CorrectionRecord, CorrectionStatus, DoubleCheckItem, run_double_check,
};
pub use prompt::{PromptLibrary, PromptTemplate, Rendered, Substitutions};
pub use provider::{
    CompletionRequest, CompletionResponse, FinishReason, LlmError, LlmProvider, MockMode,
    MockProvider, OpenAiCompatibleConfig, OpenAiCompatibleProvider, ProviderCapabilities,
    RequestMetadata, ResponseFormat,
};
pub use qa_batch::qa_segments_parallel;
pub use rate_controller::{
    ProviderRateController, RateControllerConfig, RequestObservation, RequestStatus,
};
pub use scheduler::{
    CompletedContext, ContextRegistry, ContextRunConfig, GlossaryRunConfig, QaIssue,
    QaSegmentReview, SegmentTranslation, TranslationRunConfig, qa_segments, translate_segments,
    translate_segments_with_callback,
};
pub use telemetry::{TelemetryLog, telemetry_summary};
