use bookforge_core::{
    config::{BatchConfig, ProviderRequestMetric, TranslationProfile},
    glossary::GlossaryFormat,
    ir::{BlockId, ProtectedSpan, ProtectedSpanKind, QaFindingSeverity},
    segment::{BlockTranslation, Segment, SegmentId, SegmentStatus, SegmentTextRun},
};
use std::collections::{HashMap, VecDeque, hash_map::Entry};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};
use tokio::{
    sync::{Semaphore, TryAcquireError, mpsc},
    task::JoinSet,
};

use crate::{
    CompletionRequest, FinishReason, LlmError, LlmProvider, PromptLibrary, ProviderRateController,
    RequestMetadata, RequestStatus, ResponseFormat, SegmentTranslation, Substitutions,
    TelemetryLog, TranslationRunConfig,
    concurrency::{AdaptiveLimiter, AdaptivePermit, PauseSignal, PauseState},
};

mod escalation;
mod execution;
mod planning;
mod rendering;

#[cfg(test)]
use escalation::batch_max_output_tokens;
use escalation::{
    TruncationAlertState, capped_batch_max_output_tokens, next_escalated_batch_max_output_tokens,
};
#[cfg(test)]
use execution::{
    BatchTranslationRequest, repair_batch_item_limit, request_status_for_controller,
    translate_one_batch,
};
pub use execution::{
    collect_repair_items, translate_batches_with_callback, translate_batches_with_control,
};
#[cfg(test)]
use planning::repack_batch;
pub use planning::{account_for_batch_prompt_overhead, build_translation_batches, split_batch};
use planning::{
    adaptive_sizer_mut, increment_batch_item_attempts, normalize_batch_for_current_sizer,
    repartition_pending_batches, set_batch_output_override, split_batch_with_config,
    take_batch_output_override, token_estimate,
};
pub use rendering::batch_item_validation_error;
pub use rendering::parse_batch_response;
use rendering::{
    batch_prompt_template, batch_response_item_count, parse_batch_response_with_validation,
    render_batch_prompt,
};
#[cfg(test)]
use rendering::{render_batch_items, render_batch_prompt_extra};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BatchMode {
    Plain,
    MarkerSafe,
    RunPreserving,
    TurboTextOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BatchKind {
    Translation,
    Repair,
}

#[derive(Debug, Clone)]
pub struct TranslationBatch {
    pub id: String,
    pub ordinal: usize,
    pub mode: BatchMode,
    pub kind: BatchKind,
    pub items: Vec<TranslationBatchItem>,
    pub token_estimate: usize,
    /// All items in a batch belong to the same chapter — guaranteed by
    /// [`build_translation_batches`] partitioning by `section_id` before
    /// it groups by mode and token budget. Sliding-context fencing
    /// (PR1) relies on this invariant: a batch awaits context for its
    /// earliest item once and reuses the same block for every item.
    pub section_id: bookforge_core::ir::SectionId,
}

#[derive(Debug, Clone)]
pub struct TranslationBatchItem {
    pub item_id: String,
    pub segment_id: SegmentId,
    pub section_id: bookforge_core::ir::SectionId,
    pub block_id: BlockId,
    pub ordinal: usize,
    pub kind: String,
    pub source_text: String,
    pub text_runs: Vec<SegmentTextRun>,
    pub protected_spans: Vec<ProtectedSpan>,
    pub required_markers: Vec<String>,
    pub checksum: String,
}

#[derive(Debug, Clone)]
pub struct BatchTranslationResult {
    pub batch_id: String,
    pub translations: Vec<BatchItemTranslation>,
    pub failures: Vec<BatchItemFailure>,
    pub input_tokens: Option<u64>,
    pub input_cached_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct BatchItemTranslation {
    pub item_id: String,
    pub segment_id: SegmentId,
    pub text: String,
    /// Severity-prefixed findings carried by a successful item. Only warning
    /// violations can reach this field.
    pub warning: Option<String>,
    pub input_tokens: Option<u64>,
    pub input_cached_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub tokens_estimated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchItemValidationViolation {
    pub severity: QaFindingSeverity,
    pub protected_span_kind: Option<ProtectedSpanKind>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchItemValidationError {
    violations: Vec<BatchItemValidationViolation>,
    message: String,
}

impl BatchItemValidationError {
    fn new(violations: Vec<BatchItemValidationViolation>) -> Option<Self> {
        if violations.is_empty() {
            return None;
        }
        let message = violations
            .iter()
            .map(|violation| violation.message.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        Some(Self {
            violations,
            message,
        })
    }

    pub fn violations(&self) -> &[BatchItemValidationViolation] {
        &self.violations
    }

    pub fn has_errors(&self) -> bool {
        self.violations
            .iter()
            .any(|violation| violation.severity == QaFindingSeverity::Error)
    }

    fn persistence_message(&self) -> String {
        self.violations
            .iter()
            .map(|violation| format!("{}: {}", violation.severity.as_str(), violation.message))
            .collect::<Vec<_>>()
            .join("; ")
    }

    pub fn contains(&self, pattern: &str) -> bool {
        self.message.contains(pattern)
    }
}

impl std::fmt::Display for BatchItemValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::ops::Deref for BatchItemValidationError {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.message
    }
}

#[derive(Debug, Clone)]
pub struct BatchItemFailure {
    pub item_id: String,
    pub segment_id: SegmentId,
    pub error: String,
    pub input_tokens: Option<u64>,
    pub input_cached_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub tokens_estimated: bool,
}

#[derive(Clone)]
pub struct BatchSizer {
    modes: HashMap<BatchMode, BatchModeSizing>,
    default_target_tokens: usize,
    default_max_items: usize,
    progress: Option<Arc<dyn bookforge_core::ProgressSink>>,
}

#[derive(Debug, Clone)]
pub struct BatchModeSizing {
    target_tokens: usize,
    max_items: usize,
    #[allow(dead_code)]
    initial_target_tokens: usize,
    #[allow(dead_code)]
    initial_max_items: usize,
    min_tokens: usize,
    max_tokens: usize,
    min_items: usize,
    max_items_cap: usize,
    recent: VecDeque<BatchSizingObservation>,
    last_increase: Option<Instant>,
    last_decrease: Option<Instant>,
}

#[derive(Debug, Clone, Copy)]
pub enum BatchSizingObservation {
    Success { latency_ms: u64 },
    Truncation,
    InvalidJson,
    HighLatency { latency_ms: u64 },
    OtherFailure,
}

const BATCH_SIZER_WINDOW: usize = 20;
const BATCH_SIZER_STABLE_SUCCESS_THRESHOLD: f64 = 0.98;
const BATCH_SIZER_INCREASE_INTERVAL: Duration = Duration::from_secs(5);
const BATCH_SIZER_DECREASE_INTERVAL: Duration = Duration::from_secs(1);
const BATCH_SIZER_TARGET_P95_LATENCY_MS: u64 = 30_000;
const SYSTEMIC_TRUNCATION_ALERT_AFTER: usize = 3;

#[cfg(test)]
mod tests;
