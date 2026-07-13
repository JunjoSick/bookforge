use bookforge_core::{
    config::{BatchConfig, ProviderRequestMetric, TranslationProfile},
    glossary::GlossaryFormat,
    ir::BlockId,
    segment::{BlockTranslation, Segment, SegmentId, SegmentStatus, SegmentTextRun},
};
use serde::Deserialize;
use std::collections::{HashMap, VecDeque, hash_map::Entry};
use std::sync::Arc;
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

mod planning;

#[cfg(test)]
use planning::repack_batch;
pub use planning::{account_for_batch_prompt_overhead, build_translation_batches, split_batch};
use planning::{
    adaptive_sizer_mut, increment_batch_item_attempts, normalize_batch_for_current_sizer,
    repartition_pending_batches, set_batch_output_override, split_batch_with_config,
    take_batch_output_override, token_estimate,
};

enum BatchWorkerResult {
    Provider(Result<BatchTranslationResult, LlmError>),
    StoppedUnfinished,
}

struct BatchWorkerOutput {
    batch: TranslationBatch,
    result: BatchWorkerResult,
    request_status: RequestStatus,
    latency_ms: u64,
    max_output_tokens: u32,
    output_escalated: bool,
    next_max_output_tokens: Option<u32>,
    request_permit: Option<AdaptivePermit>,
}

struct RepairWorkerOutput {
    batch: TranslationBatch,
    result: Result<BatchTranslationResult, LlmError>,
    latency_ms: u64,
    max_output_tokens: u32,
}

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
    pub protected_spans: Vec<String>,
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
    pub input_tokens: Option<u64>,
    pub input_cached_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub tokens_estimated: bool,
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

#[derive(Default)]
struct TruncationAlertState {
    unresolved_after_escalation: usize,
    alert_emitted: bool,
}

impl TruncationAlertState {
    fn observe_resolved(&mut self) {
        self.unresolved_after_escalation = 0;
    }

    fn observe_unresolved(&mut self, progress: &Arc<dyn bookforge_core::ProgressSink>) {
        self.unresolved_after_escalation = self.unresolved_after_escalation.saturating_add(1);
        if self.alert_emitted || self.unresolved_after_escalation < SYSTEMIC_TRUNCATION_ALERT_AFTER
        {
            return;
        }
        self.alert_emitted = true;
        progress.emit(bookforge_core::ProgressEvent::Warning {
            kind: "systemic_truncation".to_string(),
            message: "output budget repeatedly exhausted after escalation; raise --batch-max-output-tokens, lower --batch-max-items, or try a different model".to_string(),
            timestamp_ms: bookforge_core::progress::now_ms(),
        });
    }
}

fn batch_item_validation_error(
    item: &TranslationBatchItem,
    translation: &str,
    validate_source_copy: bool,
    section_title: Option<&str>,
) -> Option<String> {
    if let Some(error) = bookforge_core::marker::marker_structure_error(translation) {
        return Some(error);
    }
    let expected = bookforge_core::marker::marker_ids_in_text(&item.source_text);
    let actual = bookforge_core::marker::marker_ids_in_text(translation);
    for marker in &expected {
        let count = actual.iter().filter(|found| *found == marker).count();
        if count == 0 {
            return Some(format!("inline marker missing: {marker}"));
        }
        if count > 1 {
            return Some(format!("inline marker duplicated: {marker}"));
        }
    }
    for marker in &actual {
        if !expected.contains(marker) {
            return Some(format!("unknown inline marker: {marker}"));
        }
    }
    for span in &item.protected_spans {
        if !span.trim().is_empty() && !crate::validation::protected_span_present(span, translation)
        {
            return Some(format!("protected span missing: {span}"));
        }
    }
    if let Some(error) = source_copy_error(item, translation, validate_source_copy, section_title) {
        return Some(error);
    }
    None
}

fn source_copy_error(
    item: &TranslationBatchItem,
    translation: &str,
    validate_source_copy: bool,
    section_title: Option<&str>,
) -> Option<String> {
    if !validate_source_copy {
        return None;
    }
    crate::validation::source_copy_validation_error(&item.source_text, translation, section_title)
}

impl TranslationBatchItem {
    pub fn mode(&self) -> BatchMode {
        if self.text_runs.len() > 12 {
            return BatchMode::RunPreserving;
        }
        if !self.required_markers.is_empty() || !self.protected_spans.is_empty() {
            return BatchMode::MarkerSafe;
        }
        BatchMode::Plain
    }
}

#[derive(Debug, Deserialize)]
struct BatchTextResponse {
    items: Vec<BatchTextItem>,
}

#[derive(Debug, Deserialize)]
struct BatchTextItem {
    id: String,
    translation: String,
}

#[derive(Debug, Deserialize)]
struct BatchRunResponse {
    items: Vec<BatchRunItem>,
}

#[derive(Debug, Deserialize)]
struct BatchRunItem {
    id: String,
    runs: Vec<BatchRunOutput>,
}

#[derive(Debug, Deserialize)]
struct BatchRunOutput {
    id: String,
    text: String,
}

pub fn parse_batch_response(
    batch: &TranslationBatch,
    response_json: &str,
) -> Result<BatchTranslationResult, String> {
    parse_batch_response_with_validation(batch, response_json, false, None)
}

fn parse_batch_response_with_validation(
    batch: &TranslationBatch,
    response_json: &str,
    validate_source_copy: bool,
    section_titles: Option<&HashMap<String, String>>,
) -> Result<BatchTranslationResult, String> {
    let content = response_json.trim();

    match batch.mode {
        BatchMode::Plain | BatchMode::MarkerSafe | BatchMode::TurboTextOnly => {
            parse_text_batch_response(
                batch,
                content,
                batch.mode == BatchMode::TurboTextOnly,
                validate_source_copy,
                section_titles,
            )
        }
        BatchMode::RunPreserving => {
            parse_run_batch_response(batch, content, validate_source_copy, section_titles)
        }
    }
}

fn parse_text_batch_response(
    batch: &TranslationBatch,
    content: &str,
    turbo: bool,
    validate_source_copy: bool,
    section_titles: Option<&HashMap<String, String>>,
) -> Result<BatchTranslationResult, String> {
    let parsed: BatchTextResponse =
        serde_json::from_str(content).map_err(|e| format!("invalid batch JSON: {e}"))?;

    let requested_ids: HashMap<&str, &TranslationBatchItem> = batch
        .items
        .iter()
        .map(|item| (item.item_id.as_str(), item))
        .collect();

    let mut seen = HashMap::new();
    let mut translations = Vec::new();
    let mut failures = Vec::new();

    for item in &parsed.items {
        if seen.contains_key(item.id.as_str()) {
            failures.push(BatchItemFailure {
                item_id: item.id.clone(),
                segment_id: SegmentId("unknown".to_string()),
                error: "duplicate item ID in batch response".to_string(),
                input_tokens: None,
                input_cached_tokens: None,
                output_tokens: None,
                tokens_estimated: false,
            });
            continue;
        }
        seen.insert(item.id.as_str(), ());

        let Some(request_item) = requested_ids.get(item.id.as_str()) else {
            continue;
        };

        if item.translation.is_empty() && !request_item.source_text.is_empty() {
            failures.push(BatchItemFailure {
                item_id: item.id.clone(),
                segment_id: request_item.segment_id.clone(),
                error: "empty translation for non-empty source".to_string(),
                input_tokens: None,
                input_cached_tokens: None,
                output_tokens: None,
                tokens_estimated: false,
            });
            continue;
        }

        let translation = item.translation.clone();
        let section_title = section_titles
            .and_then(|titles| titles.get(&request_item.segment_id.0))
            .map(String::as_str);
        let validation_error = if turbo {
            source_copy_error(
                request_item,
                &translation,
                validate_source_copy,
                section_title,
            )
        } else {
            batch_item_validation_error(
                request_item,
                &translation,
                validate_source_copy,
                section_title,
            )
        };
        if let Some(error) = validation_error {
            failures.push(BatchItemFailure {
                item_id: item.id.clone(),
                segment_id: request_item.segment_id.clone(),
                error,
                input_tokens: None,
                input_cached_tokens: None,
                output_tokens: None,
                tokens_estimated: false,
            });
            continue;
        }

        translations.push(BatchItemTranslation {
            item_id: item.id.clone(),
            segment_id: request_item.segment_id.clone(),
            text: translation,
            input_tokens: None,
            input_cached_tokens: None,
            output_tokens: None,
            tokens_estimated: false,
        });
    }

    for item in &batch.items {
        if !seen.contains_key(item.item_id.as_str()) {
            failures.push(BatchItemFailure {
                item_id: item.item_id.clone(),
                segment_id: item.segment_id.clone(),
                error: "item missing from batch response".to_string(),
                input_tokens: None,
                input_cached_tokens: None,
                output_tokens: None,
                tokens_estimated: false,
            });
        }
    }

    Ok(BatchTranslationResult {
        batch_id: batch.id.clone(),
        translations,
        failures,
        input_tokens: None,
        input_cached_tokens: None,
        output_tokens: None,
    })
}

fn parse_run_batch_response(
    batch: &TranslationBatch,
    content: &str,
    validate_source_copy: bool,
    section_titles: Option<&HashMap<String, String>>,
) -> Result<BatchTranslationResult, String> {
    let parsed: BatchRunResponse =
        serde_json::from_str(content).map_err(|e| format!("invalid batch JSON: {e}"))?;

    let requested_ids: HashMap<&str, &TranslationBatchItem> = batch
        .items
        .iter()
        .map(|item| (item.item_id.as_str(), item))
        .collect();

    let mut seen = HashMap::new();
    let mut translations = Vec::new();
    let mut failures = Vec::new();

    for item in &parsed.items {
        if seen.contains_key(item.id.as_str()) {
            failures.push(BatchItemFailure {
                item_id: item.id.clone(),
                segment_id: SegmentId("unknown".to_string()),
                error: "duplicate item ID in batch response".to_string(),
                input_tokens: None,
                input_cached_tokens: None,
                output_tokens: None,
                tokens_estimated: false,
            });
            continue;
        }
        seen.insert(item.id.as_str(), ());

        let Some(request_item) = requested_ids.get(item.id.as_str()) else {
            continue;
        };

        let expected_run_count = request_item.text_runs.len();
        if item.runs.len() != expected_run_count {
            failures.push(BatchItemFailure {
                item_id: item.id.clone(),
                segment_id: request_item.segment_id.clone(),
                error: format!(
                    "run count mismatch: expected {expected_run_count}, got {}",
                    item.runs.len()
                ),
                input_tokens: None,
                input_cached_tokens: None,
                output_tokens: None,
                tokens_estimated: false,
            });
            continue;
        }

        let expected_ids: HashMap<&str, &SegmentTextRun> = request_item
            .text_runs
            .iter()
            .map(|run| (run.id.as_str(), run))
            .collect();
        let mut run_by_id = HashMap::with_capacity(item.runs.len());
        let mut run_error = None;
        for run in &item.runs {
            if !expected_ids.contains_key(run.id.as_str()) {
                run_error = Some(format!("unknown run ID in response: {}", run.id));
                break;
            }
            if run_by_id
                .insert(run.id.as_str(), run.text.as_str())
                .is_some()
            {
                run_error = Some(format!("duplicate run ID in response: {}", run.id));
                break;
            }
        }
        if run_error.is_none() {
            for expected in &request_item.text_runs {
                if !run_by_id.contains_key(expected.id.as_str()) {
                    run_error = Some(format!("missing run ID in response: {}", expected.id));
                    break;
                }
                if bookforge_core::marker::is_marker_token(&expected.text)
                    && run_by_id.get(expected.id.as_str()).copied() != Some(expected.text.as_str())
                {
                    run_error = Some(format!("changed marker run '{}'", expected.id));
                    break;
                }
            }
        }
        if let Some(error) = run_error {
            failures.push(BatchItemFailure {
                item_id: item.id.clone(),
                segment_id: request_item.segment_id.clone(),
                error,
                input_tokens: None,
                input_cached_tokens: None,
                output_tokens: None,
                tokens_estimated: false,
            });
            continue;
        }

        let joined: Vec<String> = request_item
            .text_runs
            .iter()
            .map(|run| {
                run_by_id
                    .get(run.id.as_str())
                    .copied()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();
        let translation = joined.join("");
        let section_title = section_titles
            .and_then(|titles| titles.get(&request_item.segment_id.0))
            .map(String::as_str);
        if let Some(error) = batch_item_validation_error(
            request_item,
            &translation,
            validate_source_copy,
            section_title,
        ) {
            failures.push(BatchItemFailure {
                item_id: item.id.clone(),
                segment_id: request_item.segment_id.clone(),
                error,
                input_tokens: None,
                input_cached_tokens: None,
                output_tokens: None,
                tokens_estimated: false,
            });
            continue;
        }
        translations.push(BatchItemTranslation {
            item_id: item.id.clone(),
            segment_id: request_item.segment_id.clone(),
            text: translation,
            input_tokens: None,
            input_cached_tokens: None,
            output_tokens: None,
            tokens_estimated: false,
        });
    }

    for item in &batch.items {
        if !seen.contains_key(item.item_id.as_str()) {
            failures.push(BatchItemFailure {
                item_id: item.item_id.clone(),
                segment_id: item.segment_id.clone(),
                error: "item missing from batch response".to_string(),
                input_tokens: None,
                input_cached_tokens: None,
                output_tokens: None,
                tokens_estimated: false,
            });
        }
    }

    Ok(BatchTranslationResult {
        batch_id: batch.id.clone(),
        translations,
        failures,
        input_tokens: None,
        input_cached_tokens: None,
        output_tokens: None,
    })
}

fn is_transient(err: &LlmError) -> bool {
    match err {
        LlmError::HttpStatus { status, .. } => *status == 429 || *status >= 500,
        LlmError::Http(e) => e.is_timeout() || e.is_connect() || e.is_decode() || e.is_body(),
        _ => false,
    }
}

pub fn collect_repair_items(result: &BatchTranslationResult) -> Vec<TranslationBatchItem> {
    result
        .failures
        .iter()
        .map(|f| TranslationBatchItem {
            item_id: f.item_id.clone(),
            segment_id: f.segment_id.clone(),
            // Repair items don't participate in the sliding-context fence
            // — they're JSON-syntax fixups, not new translation work. The
            // sentinel empty section_id is intentional and safe; the
            // batch driver never awaits context for Repair-kind batches.
            section_id: bookforge_core::ir::SectionId(String::new()),
            block_id: bookforge_core::ir::BlockId(String::new()),
            ordinal: 0,
            kind: String::new(),
            source_text: String::new(),
            text_runs: Vec::new(),
            protected_spans: Vec::new(),
            required_markers: Vec::new(),
            checksum: String::new(),
        })
        .collect()
}

/// Publish Failed status for each segment in a batch that hit a terminal
/// error. This unblocks the sliding-context fence so downstream batches
/// don't deadlock waiting on a segment that will never succeed.
fn unblock_fence_for_batch_failure(
    registry: Option<&crate::scheduler::ContextRegistry>,
    segments_by_id: &HashMap<String, Segment>,
    items: &[TranslationBatchItem],
) {
    let Some(registry) = registry else { return };
    let mut seen = std::collections::HashSet::<String>::new();
    for item in items {
        let key = item.segment_id.0.clone();
        if !seen.insert(key.clone()) {
            continue;
        }
        if let Some(segment) = segments_by_id.get(&key) {
            registry.pre_populate_text(segment, String::new(), SegmentStatus::Failed);
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn translate_batches_with_callback<P, F>(
    provider: P,
    batches: Vec<TranslationBatch>,
    segments: &[Segment],
    config: &TranslationRunConfig,
    telemetry: Arc<TelemetryLog>,
    rate_controller: Option<Arc<ProviderRateController>>,
    batch_sizer: Option<&mut BatchSizer>,
    progress: Arc<dyn bookforge_core::ProgressSink>,
    finalized_tx: Option<mpsc::Sender<SegmentTranslation>>,
    on_segment: F,
) -> Result<Vec<SegmentTranslation>, LlmError>
where
    P: LlmProvider,
    F: FnMut(&SegmentTranslation) -> Result<(), LlmError>,
{
    translate_batches_with_control(
        provider,
        batches,
        segments,
        config,
        telemetry,
        rate_controller,
        batch_sizer,
        progress,
        finalized_tx,
        on_segment,
        |_| Ok(()),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn translate_batches_with_control<P, F, C>(
    provider: P,
    batches: Vec<TranslationBatch>,
    segments: &[Segment],
    config: &TranslationRunConfig,
    telemetry: Arc<TelemetryLog>,
    rate_controller: Option<Arc<ProviderRateController>>,
    mut batch_sizer: Option<&mut BatchSizer>,
    progress: Arc<dyn bookforge_core::ProgressSink>,
    finalized_tx: Option<mpsc::Sender<SegmentTranslation>>,
    mut on_segment: F,
    mut on_control_boundary: C,
) -> Result<Vec<SegmentTranslation>, LlmError>
where
    P: LlmProvider,
    F: FnMut(&SegmentTranslation) -> Result<(), LlmError>,
    C: FnMut(&PauseSignal) -> Result<(), LlmError>,
{
    let library = Arc::new(PromptLibrary::global().clone());
    let provider = Arc::new(provider);
    let validate_source_copy = crate::validation::should_validate_source_copy(
        &config.provider,
        config.source_language.as_deref(),
        &config.target_language,
    );
    let section_titles = Arc::new(
        segments
            .iter()
            .filter_map(|segment| {
                segment
                    .metadata
                    .section_title
                    .as_ref()
                    .map(|title| (segment.id.0.clone(), title.clone()))
            })
            .collect::<HashMap<_, _>>(),
    );
    let config = Arc::new(config.clone());
    let pause_signal = config.pause_signal.clone();
    let initial_concurrency = config.scheduler.concurrency.max(1);
    let request_limiter = Arc::new(AdaptiveLimiter::new_with_bounds(
        initial_concurrency,
        1,
        Semaphore::MAX_PERMITS,
        Duration::ZERO,
        Some(progress.clone()),
    ));

    let all_items: HashMap<String, TranslationBatchItem> = batches
        .iter()
        .flat_map(|b| b.items.iter())
        .map(|item| (item.item_id.clone(), item.clone()))
        .collect();

    // Sliding-context fence (PR5): publish per-segment as soon as all of a
    // segment's blocks have arrived from one or more batches. We buffer
    // per-segment blocks until the count matches the expected block count,
    // then push the joined text into the context registry so later batches
    // can read it as prior context.
    let segments_by_id: HashMap<String, Segment> = segments
        .iter()
        .map(|s| (s.id.0.clone(), s.clone()))
        .collect();
    let segment_block_expected: HashMap<String, usize> = segments
        .iter()
        .map(|s| (s.id.0.clone(), s.block_ids.len()))
        .collect();
    let mut pending_segment_blocks: HashMap<String, HashMap<BlockId, String>> = HashMap::new();

    let mut all_results: Vec<BatchTranslationResult> = Vec::new();
    let mut pending: Vec<TranslationBatch> = batches;
    let max_rounds = 3usize;
    let mut single_invalid_attempts_by_item: HashMap<String, usize> = HashMap::new();
    let mut transient_attempts_by_item: HashMap<String, usize> = HashMap::new();
    let mut escalated_output_tokens_by_item: HashMap<String, u32> = HashMap::new();
    let mut truncation_alert = TruncationAlertState::default();
    let mut stop_dispatch = false;
    let mut runtime_sizer: Option<(u64, bool, BatchSizer)> = None;
    let mut repartitioned_revision: Option<u64> = None;

    for _round in 0..max_rounds {
        if pending.is_empty() || stop_dispatch {
            break;
        }

        // Spawn one task per queued batch, but gate provider calls below with
        // request_semaphore. Context waiters must not consume provider
        // concurrency, otherwise a split prerequisite batch can be stranded
        // behind later batches that are waiting for its context.
        let mut pending_queue: VecDeque<TranslationBatch> = pending.drain(..).collect();
        let mut tasks = JoinSet::<BatchWorkerOutput>::new();

        while (!pending_queue.is_empty() && !stop_dispatch) || !tasks.is_empty() {
            while !pending_queue.is_empty() && !stop_dispatch {
                if let Some(signal) = pause_signal.as_ref() {
                    on_control_boundary(signal)?;
                    match signal.state() {
                        PauseState::Running => {}
                        PauseState::Paused => break,
                        PauseState::Stopped => {
                            stop_dispatch = true;
                            break;
                        }
                    }
                }

                let runtime_snapshot = config
                    .runtime_settings
                    .as_ref()
                    .map(|receiver| receiver.borrow().clone());
                if let Some(runtime) = runtime_snapshot.as_ref() {
                    if request_limiter.current() != runtime.concurrency {
                        request_limiter.set_target(runtime.concurrency.max(1), "runtime_config");
                    }
                    if runtime.revision > 0
                        && runtime_sizer
                            .as_ref()
                            .is_none_or(|(revision, _, _)| *revision != runtime.revision)
                    {
                        runtime_sizer = Some((
                            runtime.revision,
                            runtime.batch.adaptive_sizing,
                            BatchSizer::with_progress(
                                runtime.batch.target_tokens,
                                runtime.batch.max_items,
                                progress.clone(),
                            ),
                        ));
                    }
                    if runtime.revision > 0
                        && repartitioned_revision != Some(runtime.revision)
                        && let Some((_, _, sizer)) = runtime_sizer.as_ref()
                    {
                        repartition_pending_batches(
                            &mut pending_queue,
                            sizer,
                            Some(config.as_ref()),
                            runtime.revision,
                        );
                        repartitioned_revision = Some(runtime.revision);
                    }
                }

                let dispatch_concurrency = runtime_snapshot
                    .as_ref()
                    .map(|runtime| runtime.concurrency)
                    .unwrap_or(config.scheduler.concurrency)
                    .max(1);
                if tasks.len() >= dispatch_concurrency {
                    break;
                }

                let Some(batch) = pending_queue.pop_front() else {
                    break;
                };
                let pending_output_override =
                    take_batch_output_override(&mut escalated_output_tokens_by_item, &batch);
                let active_sizer = runtime_sizer
                    .as_ref()
                    .map(|(_, _, sizer)| sizer)
                    .or(batch_sizer.as_deref());
                let mut normalized =
                    normalize_batch_for_current_sizer(batch, active_sizer, Some(config.as_ref()));
                if let Some(output_override) = pending_output_override {
                    for part in &normalized {
                        set_batch_output_override(
                            &mut escalated_output_tokens_by_item,
                            part,
                            output_override,
                        );
                    }
                }
                let batch = normalized.remove(0);
                let output_override =
                    take_batch_output_override(&mut escalated_output_tokens_by_item, &batch);
                for extra in normalized.into_iter().rev() {
                    pending_queue.push_front(extra);
                }
                progress.emit(bookforge_core::ProgressEvent::BatchQueued {
                    batch_id: batch.id.clone(),
                    item_count: batch.items.len(),
                    timestamp_ms: bookforge_core::progress::now_ms(),
                });

                let provider = provider.clone();
                let library = library.clone();
                let config = config.clone();
                let runtime_settings = config.runtime_settings.clone();
                let rate_controller = rate_controller.clone();
                let progress = progress.clone();
                let request_limiter = request_limiter.clone();
                let section_titles = section_titles.clone();
                let pause_signal = pause_signal.clone();

                tasks.spawn(async move {
                    let output_escalated = output_override.is_some();
                    // Strict context must be awaited before any permit is
                    // held (waiters would starve prerequisite batches);
                    // best-effort context is snapshotted after permits so
                    // earlier batches have had time to publish.
                    let strict_context_pairs = if config.context.strict {
                        Some(context_pairs_for_batch(&batch, &config).await)
                    } else {
                        None
                    };
                    if let Some(signal) = pause_signal.as_ref()
                        && signal.wait_until_running_or_stopped().await == PauseState::Stopped
                    {
                        return BatchWorkerOutput {
                            batch,
                            result: BatchWorkerResult::StoppedUnfinished,
                            request_status: RequestStatus::OtherError,
                            latency_ms: 0,
                            max_output_tokens: 0,
                            output_escalated,
                            next_max_output_tokens: None,
                            request_permit: None,
                        };
                    }
                    let request_permit = loop {
                        if let Some(receiver) = runtime_settings.as_ref() {
                            let target = receiver.borrow().concurrency.max(1);
                            if request_limiter.current() != target {
                                request_limiter.set_target(target, "runtime_config");
                            }
                        }
                        if let Some(signal) = pause_signal.as_ref()
                            && signal.wait_until_running_or_stopped().await == PauseState::Stopped
                        {
                            return BatchWorkerOutput {
                                batch,
                                result: BatchWorkerResult::StoppedUnfinished,
                                request_status: RequestStatus::OtherError,
                                latency_ms: 0,
                                max_output_tokens: 0,
                                output_escalated,
                                next_max_output_tokens: None,
                                request_permit: None,
                            };
                        }
                        match request_limiter.try_acquire() {
                            Ok(permit) => break permit,
                            Err(TryAcquireError::NoPermits) => {
                                tokio::time::sleep(Duration::from_millis(25)).await;
                            }
                            Err(TryAcquireError::Closed) => {
                                return BatchWorkerOutput {
                                    batch,
                                    result: BatchWorkerResult::Provider(Err(LlmError::Provider(
                                        "batch request semaphore closed".to_string(),
                                    ))),
                                    request_status: RequestStatus::OtherError,
                                    latency_ms: 0,
                                    max_output_tokens: 0,
                                    output_escalated,
                                    next_max_output_tokens: None,
                                    request_permit: None,
                                };
                            }
                        }
                    };

                    let adaptive_concurrency = runtime_settings
                        .as_ref()
                        .map(|receiver| receiver.borrow().adaptive_concurrency)
                        // Existing library callers express the enabled state by
                        // supplying a controller and no runtime receiver.
                        .unwrap_or_else(|| rate_controller.is_some());
                    let permit = match rate_controller.as_ref().filter(|_| adaptive_concurrency) {
                        Some(controller) => match controller.acquire().await {
                            Ok(permit) => Some(permit),
                            Err(_) => {
                                return BatchWorkerOutput {
                                    batch,
                                    result: BatchWorkerResult::Provider(Err(LlmError::Provider(
                                        "adaptive concurrency limiter closed".to_string(),
                                    ))),
                                    request_status: RequestStatus::OtherError,
                                    latency_ms: 0,
                                    max_output_tokens: 0,
                                    output_escalated,
                                    next_max_output_tokens: None,
                                    request_permit: Some(request_permit),
                                };
                            }
                        },
                        None => None,
                    };

                    let mut effective_config = config.as_ref().clone();
                    if let Some(receiver) = runtime_settings.as_ref() {
                        let runtime = receiver.borrow().clone();
                        effective_config.scheduler.concurrency = runtime.concurrency.max(1);
                        effective_config.batch_max_output_tokens = runtime.batch_max_output_tokens;
                        effective_config.runtime_settings = Some(runtime.frozen_receiver());
                    }
                    let config = Arc::new(effective_config);

                    let context_pairs = match strict_context_pairs {
                        Some(pairs) => pairs,
                        None => context_pairs_for_batch(&batch, &config).await,
                    };

                    let started = std::time::Instant::now();
                    let is_reasoning = provider.is_reasoning();
                    let default_max_output_tokens =
                        capped_batch_max_output_tokens(&batch, &config, is_reasoning);
                    let max_output_tokens = output_override.unwrap_or(default_max_output_tokens);
                    let next_max_output_tokens = (!output_escalated)
                        .then(|| {
                            next_escalated_batch_max_output_tokens(
                                max_output_tokens,
                                &batch,
                                &config,
                                is_reasoning,
                            )
                        })
                        .flatten();

                    let request_id = format!("batch_{}", batch.id);
                    let (runtime_config_revision, provider_max_attempts) =
                        config.request_runtime_metadata();
                    progress.emit(bookforge_core::ProgressEvent::RequestStarted {
                        request_id: request_id.clone(),
                        batch_id: Some(batch.id.clone()),
                        segment_id: None,
                        provider: Some(config.provider.clone()),
                        model: Some(config.model.clone()),
                        prompt_template: None,
                        items: batch.items.len(),
                        estimated_input_tokens: batch.token_estimate,
                        max_output_tokens: Some(max_output_tokens),
                        active_requests: 0,
                        target_concurrency: config.scheduler.concurrency,
                        runtime_config_revision,
                        provider_max_attempts,
                        timestamp_ms: bookforge_core::progress::now_ms(),
                    });

                    let result = translate_one_batch(
                        provider.clone(),
                        library.clone(),
                        batch.clone(),
                        &config,
                        Some(max_output_tokens),
                        context_pairs,
                        validate_source_copy,
                        &section_titles,
                    )
                    .await;
                    let latency_ms = started.elapsed().as_millis() as u64;

                    let request_status = request_status_for_controller(&result);

                    drop(permit);
                    BatchWorkerOutput {
                        batch,
                        result: BatchWorkerResult::Provider(result),
                        request_status,
                        latency_ms,
                        max_output_tokens,
                        output_escalated,
                        next_max_output_tokens,
                        request_permit: Some(request_permit),
                    }
                });
            }

            if tasks.is_empty() {
                if stop_dispatch || pending_queue.is_empty() {
                    continue;
                }
                if let Some(signal) = pause_signal.as_ref()
                    && signal.state() == PauseState::Paused
                    && wait_for_batch_resume_or_stop(signal, &mut on_control_boundary).await?
                        == PauseState::Stopped
                {
                    stop_dispatch = true;
                }
                continue;
            }

            let joined = match pause_signal.as_ref() {
                Some(signal) if signal.state() == PauseState::Paused => {
                    tokio::select! {
                        joined = tasks.join_next() => joined,
                        state = wait_for_batch_resume_or_stop(signal, &mut on_control_boundary) => {
                            if state? == PauseState::Stopped {
                                stop_dispatch = true;
                            }
                            tasks.join_next().await
                        }
                    }
                }
                _ => tasks.join_next().await,
            };
            let Some(joined) = joined else {
                continue;
            };
            let BatchWorkerOutput {
                batch,
                result,
                request_status,
                latency_ms,
                max_output_tokens,
                output_escalated,
                next_max_output_tokens,
                request_permit,
            } = joined
                .map_err(|err| LlmError::Provider(format!("batch worker task failed: {err}")))?;

            if let Some(signal) = pause_signal.as_ref() {
                on_control_boundary(signal)?;
                if signal.state() == PauseState::Stopped {
                    stop_dispatch = true;
                }
            }
            drop(request_permit);

            let result = match result {
                BatchWorkerResult::Provider(result) => result,
                BatchWorkerResult::StoppedUnfinished => {
                    stop_dispatch = true;
                    unblock_fence_for_batch_failure(
                        config.context_registry.as_deref(),
                        &segments_by_id,
                        &batch.items,
                    );
                    continue;
                }
            };

            progress.emit(bookforge_core::ProgressEvent::RequestFinished {
                request_id: format!("batch_{}", batch.id),
                batch_id: Some(batch.id.clone()),
                segment_id: None,
                status: result
                    .as_ref()
                    .map_or_else(|e| request_status_from_error(e), |_| "ok")
                    .to_string(),
                latency_ms,
                status_code: None,
                finish_reason: None,
                retry_count: 0,
                input_tokens: result.as_ref().ok().and_then(|r| r.input_tokens),
                output_tokens: result.as_ref().ok().and_then(|r| r.output_tokens),
                error_kind: result.as_ref().err().map(|e| format!("{e:?}")),
                timestamp_ms: bookforge_core::progress::now_ms(),
            });

            telemetry.record(ProviderRequestMetric {
                request_id: format!("batch_{}", batch.id),
                batch_id: Some(batch.id.clone()),
                provider: config.provider.clone(),
                model: config.model.clone(),
                profile: config.profile.namespace_str().to_string(),
                items: batch.items.len(),
                estimated_input_tokens: batch.token_estimate,
                max_output_tokens: Some(max_output_tokens),
                input_tokens: result.as_ref().ok().and_then(|r| r.input_tokens),
                output_tokens: result.as_ref().ok().and_then(|r| r.output_tokens),
                latency_ms,
                finish_reason: None,
                status: if result.is_ok() {
                    "ok".into()
                } else {
                    "error".into()
                },
                status_code: None,
                retry_count: 0,
                backoff_ms: 0,
                error_kind: None,
            });

            let adaptive_concurrency = config
                .runtime_settings
                .as_ref()
                .map(|receiver| receiver.borrow().adaptive_concurrency)
                .unwrap_or_else(|| rate_controller.is_some());
            if let Some(controller) = rate_controller.as_ref().filter(|_| adaptive_concurrency) {
                controller.observe(request_status, latency_ms);
            }

            match result {
                Ok(batch_result) => {
                    truncation_alert.observe_resolved();
                    if let Some(sizer) =
                        adaptive_sizer_mut(&mut runtime_sizer, batch_sizer.as_deref_mut())
                    {
                        sizer.on_success_for_mode(batch.mode, latency_ms);
                    }
                    // Publish completed segments to the context registry as
                    // soon as all of their blocks have landed. Repair batches
                    // don't participate (they're fixing prior translations,
                    // not producing new ones — and they carry the sentinel
                    // empty section_id).
                    if let Some(registry) = config.context_registry.as_deref()
                        && batch.kind == BatchKind::Translation
                    {
                        for item in &batch_result.translations {
                            let key = item.segment_id.0.clone();
                            let Some(source_item) = all_items.get(&item.item_id) else {
                                continue;
                            };
                            pending_segment_blocks
                                .entry(key.clone())
                                .or_default()
                                .insert(source_item.block_id.clone(), item.text.clone());
                            let expected = segment_block_expected
                                .get(&key)
                                .copied()
                                .unwrap_or(usize::MAX);
                            if pending_segment_blocks[&key].len() >= expected
                                && let Some(segment) = segments_by_id.get(&key)
                            {
                                let blocks = pending_segment_blocks.remove(&key).unwrap();
                                let joined = segment
                                    .block_ids
                                    .iter()
                                    .filter_map(|block_id| blocks.get(block_id))
                                    .cloned()
                                    .collect::<Vec<_>>()
                                    .join("\n\n");
                                registry.pre_populate_text(
                                    segment,
                                    joined,
                                    SegmentStatus::Succeeded,
                                );
                            }
                        }
                        // Failures must also unblock the fence so downstream
                        // batches don't deadlock waiting on a segment that
                        // will never publish a Succeeded entry.
                        for failure in &batch_result.failures {
                            if let Some(segment) = segments_by_id.get(&failure.segment_id.0) {
                                registry.pre_populate_text(
                                    segment,
                                    String::new(),
                                    SegmentStatus::Failed,
                                );
                            }
                        }
                    }
                    all_results.push(batch_result);
                }
                Err(LlmError::InvalidResponse(_))
                    if batch.kind == BatchKind::Translation
                        && request_status == RequestStatus::Truncated
                        && !output_escalated
                        && next_max_output_tokens.is_some() =>
                {
                    let next_max_output_tokens =
                        next_max_output_tokens.expect("checked Some above");
                    progress.emit(bookforge_core::ProgressEvent::Warning {
                        kind: "batch_truncation_escalated_retry".to_string(),
                        message: format!(
                            "batch {} exhausted max_output_tokens {}; retrying once with {} before splitting",
                            batch.id, max_output_tokens, next_max_output_tokens
                        ),
                        timestamp_ms: bookforge_core::progress::now_ms(),
                    });
                    set_batch_output_override(
                        &mut escalated_output_tokens_by_item,
                        &batch,
                        next_max_output_tokens,
                    );
                    pending_queue.push_back(batch);
                }
                Err(LlmError::InvalidResponse(_)) if batch.kind == BatchKind::Repair => {
                    truncation_alert.observe_resolved();
                    progress.emit(bookforge_core::ProgressEvent::Warning {
                        kind: "repair_batch_invalid_response".to_string(),
                        message: format!(
                            "repair batch {} failed; marking {} items NeedsReview",
                            batch.id,
                            batch.items.len()
                        ),
                        timestamp_ms: bookforge_core::progress::now_ms(),
                    });
                    // Repair batches don't participate in the fence — but
                    // their underlying segments may not be otherwise
                    // resolved, so still unblock anyone waiting on them.
                    unblock_fence_for_batch_failure(
                        config.context_registry.as_deref(),
                        &segments_by_id,
                        &batch.items,
                    );
                    all_results.push(BatchTranslationResult {
                        batch_id: batch.id.clone(),
                        translations: Vec::new(),
                        failures: batch
                            .items
                            .iter()
                            .map(|item| BatchItemFailure {
                                item_id: item.item_id.clone(),
                                segment_id: item.segment_id.clone(),
                                error: "repair batch invalid response".to_string(),
                                input_tokens: None,
                                input_cached_tokens: None,
                                output_tokens: None,
                                tokens_estimated: false,
                            })
                            .collect(),
                        input_tokens: None,
                        input_cached_tokens: None,
                        output_tokens: None,
                    });
                }
                Err(error @ LlmError::InvalidResponse(_))
                    if request_status == RequestStatus::Truncated && batch.items.len() == 1 =>
                {
                    if let Some(sizer) =
                        adaptive_sizer_mut(&mut runtime_sizer, batch_sizer.as_deref_mut())
                    {
                        sizer.on_truncation_for_mode(batch.mode);
                    }
                    truncation_alert.observe_unresolved(&progress);
                    progress.emit(bookforge_core::ProgressEvent::Warning {
                        kind: "single_item_batch_truncated".to_string(),
                        message: format!(
                            "single-item batch {} still exhausted max_output_tokens {}; not splitting further",
                            batch.id, max_output_tokens
                        ),
                        timestamp_ms: bookforge_core::progress::now_ms(),
                    });
                    unblock_fence_for_batch_failure(
                        config.context_registry.as_deref(),
                        &segments_by_id,
                        &batch.items,
                    );
                    all_results.push(BatchTranslationResult {
                        batch_id: batch.id.clone(),
                        translations: Vec::new(),
                        failures: batch
                            .items
                            .iter()
                            .map(|item| BatchItemFailure {
                                item_id: item.item_id.clone(),
                                segment_id: item.segment_id.clone(),
                                error: format!("single-item batch truncated: {error}"),
                                input_tokens: None,
                                input_cached_tokens: None,
                                output_tokens: None,
                                tokens_estimated: false,
                            })
                            .collect(),
                        input_tokens: None,
                        input_cached_tokens: None,
                        output_tokens: None,
                    });
                }
                Err(error @ LlmError::InvalidResponse(_)) if batch.items.len() == 1 => {
                    truncation_alert.observe_resolved();
                    let attempts =
                        increment_batch_item_attempts(&mut single_invalid_attempts_by_item, &batch);
                    if attempts < config.scheduler.max_attempts.max(1) {
                        progress.emit(bookforge_core::ProgressEvent::Warning {
                            kind: "single_item_batch_invalid_response_retry".to_string(),
                            message: format!(
                                "single-item batch {} returned invalid response on attempt {}; retrying: {error}",
                                batch.id, attempts
                            ),
                            timestamp_ms: bookforge_core::progress::now_ms(),
                        });
                        pending_queue.push_back(batch);
                    } else {
                        progress.emit(bookforge_core::ProgressEvent::Warning {
                            kind: "single_item_batch_invalid_response".to_string(),
                            message: format!(
                                "single-item batch {} failed after {} attempts; not splitting further",
                                batch.id, attempts
                            ),
                            timestamp_ms: bookforge_core::progress::now_ms(),
                        });
                        unblock_fence_for_batch_failure(
                            config.context_registry.as_deref(),
                            &segments_by_id,
                            &batch.items,
                        );
                        all_results.push(BatchTranslationResult {
                            batch_id: batch.id.clone(),
                            translations: Vec::new(),
                            failures: batch
                                .items
                                .iter()
                                .map(|item| BatchItemFailure {
                                    item_id: item.item_id.clone(),
                                    segment_id: item.segment_id.clone(),
                                    error: format!("single-item batch invalid response: {error}"),
                                    input_tokens: None,
                                    input_cached_tokens: None,
                                    output_tokens: None,
                                    tokens_estimated: false,
                                })
                                .collect(),
                            input_tokens: None,
                            input_cached_tokens: None,
                            output_tokens: None,
                        });
                    }
                }
                Err(LlmError::InvalidResponse(_)) if batch.items.len() > 1 => {
                    if let Some(sizer) =
                        adaptive_sizer_mut(&mut runtime_sizer, batch_sizer.as_deref_mut())
                    {
                        if request_status == RequestStatus::Truncated {
                            sizer.on_truncation_for_mode(batch.mode);
                        } else {
                            sizer.on_invalid_json_for_mode(batch.mode);
                        }
                    }
                    if request_status == RequestStatus::Truncated {
                        truncation_alert.observe_unresolved(&progress);
                    } else {
                        truncation_alert.observe_resolved();
                    }
                    let split = split_batch_with_config(&batch, Some(config.as_ref()));
                    if split.len() == 2 {
                        progress.emit(bookforge_core::ProgressEvent::BatchSplit {
                            batch_id: batch.id.clone(),
                            left_items: split[0].items.len(),
                            right_items: split[1].items.len(),
                            timestamp_ms: bookforge_core::progress::now_ms(),
                        });
                    }
                    progress.emit(bookforge_core::ProgressEvent::Warning {
                        kind: if request_status == RequestStatus::Truncated {
                            "batch_truncated_split"
                        } else {
                            "batch_invalid_response_split"
                        }
                        .to_string(),
                        message: format!(
                            "batch {} failed with {}, splitting",
                            batch.id,
                            if request_status == RequestStatus::Truncated {
                                "truncated output"
                            } else {
                                "invalid response"
                            }
                        ),
                        timestamp_ms: bookforge_core::progress::now_ms(),
                    });
                    pending_queue.extend(split);
                }
                Err(ref error) if is_transient(error) && batch.kind == BatchKind::Translation => {
                    truncation_alert.observe_resolved();
                    let attempts =
                        increment_batch_item_attempts(&mut transient_attempts_by_item, &batch);
                    if attempts < config.scheduler.max_attempts.max(1) {
                        progress.emit(bookforge_core::ProgressEvent::Warning {
                            kind: "batch_transient_retry".to_string(),
                            message: format!(
                                "batch {} transient error on attempt {}; retrying: {error}",
                                batch.id, attempts
                            ),
                            timestamp_ms: bookforge_core::progress::now_ms(),
                        });
                        pending_queue.push_back(batch);
                    } else {
                        progress.emit(bookforge_core::ProgressEvent::Warning {
                            kind: "batch_transient_exhausted".to_string(),
                            message: format!(
                                "batch {} failed after {} transient attempts: {error}",
                                batch.id, attempts
                            ),
                            timestamp_ms: bookforge_core::progress::now_ms(),
                        });
                        unblock_fence_for_batch_failure(
                            config.context_registry.as_deref(),
                            &segments_by_id,
                            &batch.items,
                        );
                        all_results.push(BatchTranslationResult {
                            batch_id: batch.id.clone(),
                            translations: Vec::new(),
                            failures: batch
                                .items
                                .iter()
                                .map(|item| BatchItemFailure {
                                    item_id: item.item_id.clone(),
                                    segment_id: item.segment_id.clone(),
                                    error: format!("{error}"),
                                    input_tokens: None,
                                    input_cached_tokens: None,
                                    output_tokens: None,
                                    tokens_estimated: false,
                                })
                                .collect(),
                            input_tokens: None,
                            input_cached_tokens: None,
                            output_tokens: None,
                        });
                    }
                }
                Err(error) => {
                    truncation_alert.observe_resolved();
                    progress.emit(bookforge_core::ProgressEvent::Warning {
                        kind: "batch_failed".to_string(),
                        message: format!("batch {} failed: {error}", batch.id),
                        timestamp_ms: bookforge_core::progress::now_ms(),
                    });
                    unblock_fence_for_batch_failure(
                        config.context_registry.as_deref(),
                        &segments_by_id,
                        &batch.items,
                    );
                    all_results.push(BatchTranslationResult {
                        batch_id: batch.id.clone(),
                        translations: Vec::new(),
                        failures: batch
                            .items
                            .iter()
                            .map(|item| BatchItemFailure {
                                item_id: item.item_id.clone(),
                                segment_id: item.segment_id.clone(),
                                error: format!("{error}"),
                                input_tokens: None,
                                input_cached_tokens: None,
                                output_tokens: None,
                                tokens_estimated: false,
                            })
                            .collect(),
                        input_tokens: None,
                        input_cached_tokens: None,
                        output_tokens: None,
                    });
                }
            }
        }
        if stop_dispatch {
            break;
        }
        pending = pending_queue.into();
    }

    progress.emit(bookforge_core::ProgressEvent::Warning {
        kind: "batch_finalize_started".to_string(),
        message: format!(
            "batch provider requests complete; aggregating {} batch results",
            all_results.len()
        ),
        timestamp_ms: bookforge_core::progress::now_ms(),
    });

    let mut segment_translations: HashMap<String, SegmentTranslation> = HashMap::new();

    let segments_by_id: HashMap<&str, &Segment> =
        segments.iter().map(|s| (s.id.0.as_str(), s)).collect();

    let make_entry = |seg_id: &str,
                      status: SegmentStatus,
                      error: Option<String>,
                      input_tokens: Option<u64>,
                      input_cached_tokens: Option<u64>,
                      output_tokens: Option<u64>,
                      tokens_estimated: bool|
     -> SegmentTranslation {
        if let Some(seg) = segments_by_id.get(seg_id) {
            SegmentTranslation {
                segment_id: SegmentId(seg_id.to_string()),
                ordinal: seg.ordinal,
                block_ids: seg.block_ids.clone(),
                blocks: Vec::new(),
                checksum: seg.checksum.clone(),
                status,
                template: "batch".to_string(),
                error,
                input_tokens,
                input_cached_tokens,
                output_tokens,
                tokens_estimated,
            }
        } else {
            SegmentTranslation {
                segment_id: SegmentId(seg_id.to_string()),
                ordinal: 0,
                block_ids: Vec::new(),
                blocks: Vec::new(),
                checksum: String::new(),
                status,
                template: "batch".to_string(),
                error,
                input_tokens,
                input_cached_tokens,
                output_tokens,
                tokens_estimated,
            }
        }
    };

    for batch_result in &all_results {
        for translation in &batch_result.translations {
            let seg_id = translation.segment_id.0.clone();
            let entry = segment_translations
                .entry(seg_id.clone())
                .or_insert_with(|| {
                    make_entry(
                        &seg_id,
                        SegmentStatus::Succeeded,
                        None,
                        None,
                        None,
                        None,
                        false,
                    )
                });
            add_usage(entry, translation);
            if let Some(source_item) = all_items.get(&translation.item_id) {
                entry.blocks.push(BlockTranslation {
                    block_id: source_item.block_id.clone(),
                    text: translation.text.clone(),
                });
            } else {
                progress.emit(bookforge_core::ProgressEvent::Warning {
                    kind: "batch_internal_missing_item".to_string(),
                    message: format!(
                        "batch translation item_id {} missing from all_items; skipping (internal state bug)",
                        translation.item_id
                    ),
                    timestamp_ms: bookforge_core::progress::now_ms(),
                });
            }
        }

        for failure in &batch_result.failures {
            let seg_id = failure.segment_id.0.clone();
            let entry = match segment_translations.entry(seg_id.clone()) {
                Entry::Occupied(entry) => {
                    let entry = entry.into_mut();
                    entry.status = SegmentStatus::NeedsReview;
                    append_translation_error(entry, &failure.error);
                    entry
                }
                Entry::Vacant(entry) => entry.insert(make_entry(
                    &seg_id,
                    SegmentStatus::NeedsReview,
                    Some(failure.error.clone()),
                    None,
                    None,
                    None,
                    false,
                )),
            };
            add_failure_usage(entry, failure);
        }
    }

    let repair_items: Vec<(BatchItemFailure, TranslationBatchItem)> = all_results
        .iter()
        .flat_map(|r| &r.failures)
        .filter(|f| f.segment_id.0 != "unknown")
        .filter(|f| repairable_batch_failure(f))
        .filter_map(|f| {
            all_items
                .get(f.item_id.as_str())
                .map(|item| (f.clone(), (*item).clone()))
        })
        .collect();

    progress.emit(bookforge_core::ProgressEvent::Warning {
        kind: "batch_aggregation_finished".to_string(),
        message: format!(
            "batch aggregation produced {} segment records and {} repair candidates",
            segment_translations.len(),
            repair_items.len()
        ),
        timestamp_ms: bookforge_core::progress::now_ms(),
    });

    if !repair_items.is_empty() {
        progress.emit(bookforge_core::ProgressEvent::BatchRepairStarted {
            failed_item_count: repair_items.len(),
            timestamp_ms: bookforge_core::progress::now_ms(),
        });

        let repair_errors: Arc<HashMap<String, String>> = Arc::new(
            repair_items
                .iter()
                .map(|(failure, _)| (failure.item_id.clone(), failure.error.clone()))
                .collect(),
        );
        let mut repair_batches: VecDeque<TranslationBatch> = repair_items
            .iter()
            .map(|(_, item)| item.clone())
            .collect::<Vec<_>>()
            .chunks(16)
            .enumerate()
            .map(|(idx, items)| {
                let items = items.to_vec();
                TranslationBatch {
                    id: format!("repair_{idx:04}"),
                    ordinal: idx,
                    mode: BatchMode::Plain,
                    kind: BatchKind::Repair,
                    token_estimate: items
                        .iter()
                        .map(|item| token_estimate(&item.source_text))
                        .sum(),
                    items,
                    // Repair batches don't participate in the sliding-
                    // context fence (they fix JSON syntax, not translation
                    // content); the sentinel section_id is harmless.
                    section_id: bookforge_core::ir::SectionId(String::new()),
                }
            })
            .collect();

        let mut repaired_count = 0usize;
        let mut repair_tasks = JoinSet::<RepairWorkerOutput>::new();

        while !repair_batches.is_empty() || !repair_tasks.is_empty() {
            loop {
                let runtime_snapshot = config
                    .runtime_settings
                    .as_ref()
                    .map(|receiver| receiver.borrow().clone());
                let concurrency = runtime_snapshot
                    .as_ref()
                    .map(|runtime| runtime.concurrency)
                    .unwrap_or(config.scheduler.concurrency)
                    .max(1);
                if repair_tasks.len() >= concurrency {
                    break;
                }
                let Some(repair_batch) = repair_batches.pop_front() else {
                    break;
                };
                progress.emit(bookforge_core::ProgressEvent::BatchQueued {
                    batch_id: repair_batch.id.clone(),
                    item_count: repair_batch.items.len(),
                    timestamp_ms: bookforge_core::progress::now_ms(),
                });

                let provider = provider.clone();
                let library = library.clone();
                let mut task_config = config.as_ref().clone();
                if let Some(runtime) = runtime_snapshot {
                    task_config.scheduler.concurrency = runtime.concurrency.max(1);
                    task_config.batch_max_output_tokens = runtime.batch_max_output_tokens;
                    task_config.runtime_settings = Some(runtime.frozen_receiver());
                }
                let config = Arc::new(task_config);
                let repair_errors = repair_errors.clone();
                let progress = progress.clone();
                let section_titles = section_titles.clone();

                repair_tasks.spawn(async move {
                    let started = std::time::Instant::now();
                    let is_reasoning = provider.is_reasoning();
                    let max_output_tokens =
                        capped_batch_max_output_tokens(&repair_batch, &config, is_reasoning);
                    let request_id = format!("batch_{}", repair_batch.id);
                    let (runtime_config_revision, provider_max_attempts) =
                        config.request_runtime_metadata();

                    progress.emit(bookforge_core::ProgressEvent::RequestStarted {
                        request_id: request_id.clone(),
                        batch_id: Some(repair_batch.id.clone()),
                        segment_id: None,
                        provider: Some(config.provider.clone()),
                        model: Some(config.model.clone()),
                        prompt_template: Some("batch_repair".to_string()),
                        items: repair_batch.items.len(),
                        estimated_input_tokens: repair_batch.token_estimate,
                        max_output_tokens: Some(max_output_tokens),
                        active_requests: 0,
                        target_concurrency: config.scheduler.concurrency,
                        runtime_config_revision,
                        provider_max_attempts,
                        timestamp_ms: bookforge_core::progress::now_ms(),
                    });

                    let items_json: Vec<serde_json::Value> = repair_batch
                        .items
                        .iter()
                        .map(|item| {
                            serde_json::json!({
                                "id": item.item_id,
                                "source_text": item.source_text,
                                "required_markers": item.required_markers,
                                "protected": item.protected_spans,
                            })
                        })
                        .collect();

                    let errors_json: Vec<serde_json::Value> = repair_batch
                        .items
                        .iter()
                        .map(|item| {
                            serde_json::json!({
                                "id": item.item_id,
                                "error": repair_errors
                                    .get(&item.item_id)
                                    .cloned()
                                    .unwrap_or_else(|| "invalid batch item".to_string()),
                            })
                        })
                        .collect();

                    let mut vars = Substitutions::new();
                    vars.raw(
                        "items_json",
                        serde_json::to_string(&items_json).unwrap_or_default(),
                    )
                    .raw(
                        "errors_json",
                        serde_json::to_string(&errors_json).unwrap_or_default(),
                    );

                    let repair_template = if config.compact_prompts {
                        &library.batch_repair_compact
                    } else {
                        &library.batch_repair
                    };

                    let result = match repair_template.render(&vars) {
                        Ok(rendered) => {
                            match provider
                                .complete(CompletionRequest {
                                    system: rendered.system,
                                    user: rendered.user,
                                    response_format: ResponseFormat::Json,
                                    temperature: 0.1,
                                    max_output_tokens: Some(max_output_tokens),
                                    metadata: RequestMetadata {
                                        segment_id: Some(format!("batch_{}", repair_batch.id)),
                                        block_ids: repair_batch
                                            .items
                                            .iter()
                                            .map(|item| item.block_id.0.clone())
                                            .collect(),
                                        prompt_template: Some(repair_template.name.clone()),
                                        prompt_version: Some(repair_template.version.clone()),
                                        provider: Some(config.provider.clone()),
                                        model: Some(config.model.clone()),
                                        source_checksum: None,
                                        runtime_config_revision,
                                        provider_max_attempts,
                                    },
                                })
                                .await
                            {
                                Ok(response) => {
                                    match parse_batch_response_with_validation(
                                        &repair_batch,
                                        &response.content,
                                        validate_source_copy,
                                        Some(&section_titles),
                                    ) {
                                        Ok(mut repaired) => {
                                            repaired.input_tokens = response.input_tokens;
                                            repaired.output_tokens = response.output_tokens;
                                            Ok(repaired)
                                        }
                                        Err(error) => Err(LlmError::InvalidResponse(error)),
                                    }
                                }
                                Err(error) => Err(error),
                            }
                        }
                        Err(error) => Err(LlmError::Provider(format!(
                            "failed to render repair prompt: {error}"
                        ))),
                    };

                    RepairWorkerOutput {
                        batch: repair_batch,
                        result,
                        latency_ms: started.elapsed().as_millis() as u64,
                        max_output_tokens,
                    }
                });
            }

            let Some(joined) = repair_tasks.join_next().await else {
                continue;
            };
            let RepairWorkerOutput {
                batch,
                result,
                latency_ms,
                max_output_tokens,
            } = joined
                .map_err(|err| LlmError::Provider(format!("repair worker task failed: {err}")))?;

            progress.emit(bookforge_core::ProgressEvent::RequestFinished {
                request_id: format!("batch_{}", batch.id),
                batch_id: Some(batch.id.clone()),
                segment_id: None,
                status: result
                    .as_ref()
                    .map_or_else(|e| request_status_from_error(e), |_| "ok")
                    .to_string(),
                latency_ms,
                status_code: None,
                finish_reason: None,
                retry_count: 0,
                input_tokens: result.as_ref().ok().and_then(|r| r.input_tokens),
                output_tokens: result.as_ref().ok().and_then(|r| r.output_tokens),
                error_kind: result.as_ref().err().map(|e| format!("{e:?}")),
                timestamp_ms: bookforge_core::progress::now_ms(),
            });

            telemetry.record(ProviderRequestMetric {
                request_id: format!("batch_{}", batch.id),
                batch_id: Some(batch.id.clone()),
                provider: config.provider.clone(),
                model: config.model.clone(),
                profile: config.profile.namespace_str().to_string(),
                items: batch.items.len(),
                estimated_input_tokens: batch.token_estimate,
                max_output_tokens: Some(max_output_tokens),
                input_tokens: result.as_ref().ok().and_then(|r| r.input_tokens),
                output_tokens: result.as_ref().ok().and_then(|r| r.output_tokens),
                latency_ms,
                finish_reason: None,
                status: if result.is_ok() {
                    "ok".into()
                } else {
                    "error".into()
                },
                status_code: None,
                retry_count: 0,
                backoff_ms: 0,
                error_kind: None,
            });

            match result {
                Ok(repaired) => {
                    for translation in repaired.translations {
                        let Some(source_item) = all_items.get(&translation.item_id) else {
                            continue;
                        };
                        if let Some(existing) =
                            segment_translations.get_mut(&translation.segment_id.0)
                        {
                            existing.status = SegmentStatus::Succeeded;
                            existing.error = None;
                            if let Some(block) = existing
                                .blocks
                                .iter_mut()
                                .find(|b| b.block_id == source_item.block_id)
                            {
                                block.text = translation.text;
                            } else {
                                existing.blocks.push(BlockTranslation {
                                    block_id: source_item.block_id.clone(),
                                    text: translation.text,
                                });
                            }
                            repaired_count += 1;
                        }
                    }
                }
                Err(error) => {
                    progress.emit(bookforge_core::ProgressEvent::Warning {
                        kind: "repair_batch_failed".to_string(),
                        message: format!(
                            "repair batch {} failed for {} items: {error}",
                            batch.id,
                            batch.items.len()
                        ),
                        timestamp_ms: bookforge_core::progress::now_ms(),
                    });
                }
            }
        }

        progress.emit(bookforge_core::ProgressEvent::BatchRepairFinished {
            repaired_items: repaired_count,
            still_failed_items: repair_items.len().saturating_sub(repaired_count),
            timestamp_ms: bookforge_core::progress::now_ms(),
        });
    }

    let mut translations: Vec<SegmentTranslation> = segment_translations.into_values().collect();

    for translation in &mut translations {
        let (ordered_blocks, missing, extra, duplicate) = order_blocks_by_segment(
            &translation.block_ids,
            std::mem::take(&mut translation.blocks),
        );
        translation.blocks = ordered_blocks;

        if (!missing.is_empty() || !extra.is_empty() || !duplicate.is_empty())
            && (translation.status == SegmentStatus::Succeeded || !translation.blocks.is_empty())
        {
            translation.status = SegmentStatus::NeedsReview;
            let error = format!(
                "batch translation block mismatch: missing={missing:?}, extra={extra:?}, duplicate={duplicate:?}",
            );
            append_translation_error(translation, &error);
        }
    }

    progress.emit(bookforge_core::ProgressEvent::Warning {
        kind: "batch_finalized_segments".to_string(),
        message: format!(
            "batch finalization produced {} segment translations",
            translations.len()
        ),
        timestamp_ms: bookforge_core::progress::now_ms(),
    });

    for translation in &mut translations {
        if let Some(ref tx) = finalized_tx {
            tx.send(translation.clone())
                .await
                .map_err(|_| LlmError::Provider("finalized segment channel closed".to_string()))?;
        }
        on_segment(translation)?;
    }

    Ok(translations)
}

async fn wait_for_batch_resume_or_stop<C>(
    signal: &PauseSignal,
    on_control_boundary: &mut C,
) -> Result<PauseState, LlmError>
where
    C: FnMut(&PauseSignal) -> Result<(), LlmError>,
{
    loop {
        on_control_boundary(signal)?;
        match signal.state() {
            PauseState::Running => return Ok(PauseState::Running),
            PauseState::Stopped => return Ok(PauseState::Stopped),
            PauseState::Paused => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

fn batch_max_output_tokens(
    batch: &TranslationBatch,
    profile: TranslationProfile,
    reasoning: bool,
    extended_output: bool,
) -> u32 {
    let base_multiplier = match batch.mode {
        BatchMode::Plain => 3,
        BatchMode::MarkerSafe => 4,
        BatchMode::RunPreserving => 5,
        BatchMode::TurboTextOnly => 2,
    };
    let multiplier = if reasoning {
        base_multiplier * 3
    } else {
        base_multiplier
    };
    // JSON output has a fixed envelope per item (ID, keys, quoting, commas)
    // that source-token estimates do not capture. Without this allowance,
    // batches of many short labels can receive a 512-token budget and
    // repeatedly truncate even though their prose payload is tiny.
    let envelope = 128u32.saturating_add((batch.items.len() as u32).saturating_mul(64));
    let estimate = (batch.token_estimate as u32)
        .saturating_mul(multiplier)
        .saturating_add(envelope);
    let max = if profile == TranslationProfile::FreeTier {
        if reasoning { 8_192 } else { 4_096 }
    } else if extended_output {
        32_768
    } else {
        if reasoning { 32_768 } else { 16_384 }
    };
    estimate.clamp(512, max)
}

fn capped_batch_max_output_tokens(
    batch: &TranslationBatch,
    config: &TranslationRunConfig,
    reasoning: bool,
) -> u32 {
    let extended_output = config.provider.eq_ignore_ascii_case("deepseek");
    let computed = batch_max_output_tokens(batch, config.profile, reasoning, extended_output);
    let user_cap = config.batch_max_output_tokens.or(config.max_output_tokens);
    bookforge_core::config::cap_output_tokens(
        computed,
        batch.token_estimate,
        config.model_context_tokens,
        user_cap,
    )
}

fn next_escalated_batch_max_output_tokens(
    current: u32,
    batch: &TranslationBatch,
    config: &TranslationRunConfig,
    reasoning: bool,
) -> Option<u32> {
    let ceiling = batch_output_token_ceiling(batch, config, reasoning);
    let bumped = current.saturating_mul(2).max(current.saturating_add(2_048));
    let next = bumped.min(ceiling);
    (next > current).then_some(next)
}

fn batch_output_token_ceiling(
    batch: &TranslationBatch,
    config: &TranslationRunConfig,
    reasoning: bool,
) -> u32 {
    let extended_output = config.provider.eq_ignore_ascii_case("deepseek");
    let ceiling = if config.profile == TranslationProfile::FreeTier {
        if reasoning { 8_192 } else { 4_096 }
    } else if extended_output || reasoning {
        32_768
    } else {
        16_384
    };
    bookforge_core::config::cap_output_tokens(
        ceiling,
        batch.token_estimate,
        config.model_context_tokens,
        None,
    )
}

async fn translate_one_batch(
    provider: Arc<impl LlmProvider>,
    library: Arc<PromptLibrary>,
    batch: TranslationBatch,
    config: &TranslationRunConfig,
    max_output_tokens_override: Option<u32>,
    context_pairs: Vec<crate::scheduler::CompletedContext>,
    validate_source_copy: bool,
    section_titles: &HashMap<String, String>,
) -> Result<BatchTranslationResult, LlmError> {
    let context_block = crate::scheduler::render_context_pairs(&context_pairs);
    let items_json = render_batch_items(&batch, config);
    let template = if config.compact_prompts {
        match batch.mode {
            BatchMode::Plain | BatchMode::TurboTextOnly => &library.batch_plain_compact,
            BatchMode::MarkerSafe => &library.batch_marker_safe_compact,
            BatchMode::RunPreserving => &library.batch_run_preserving_compact,
        }
    } else {
        match batch.mode {
            BatchMode::Plain | BatchMode::TurboTextOnly => &library.batch_plain,
            BatchMode::MarkerSafe => &library.batch_marker_safe,
            BatchMode::RunPreserving => &library.batch_run_preserving,
        }
    };

    let mut vars = Substitutions::new();
    vars.string(
        "source_language",
        config
            .source_language
            .as_deref()
            .unwrap_or("the source language"),
    )
    .string("target_language", &config.target_language)
    .raw(
        "style_guide_block",
        config
            .style
            .as_ref()
            .map(|s| s.rendered_block.clone())
            .unwrap_or_default(),
    )
    .raw(
        "entity_agreement_block",
        config
            .entities
            .as_ref()
            .map(|e| e.rendered_block.clone())
            .unwrap_or_default(),
    )
    .raw("context_translation_pairs", context_block)
    .raw(
        "prompt_extra",
        config.glossary.prompt_extra.clone().unwrap_or_default(),
    )
    .raw("items_json", items_json);

    let rendered = template
        .render(&vars)
        .map_err(|e| LlmError::Provider(e.to_string()))?;

    let max_tokens = max_output_tokens_override
        .unwrap_or_else(|| capped_batch_max_output_tokens(&batch, config, provider.is_reasoning()));
    let (runtime_config_revision, provider_max_attempts) = config.request_runtime_metadata();

    let response = provider
        .complete(CompletionRequest {
            system: rendered.system,
            user: rendered.user,
            response_format: ResponseFormat::Json,
            temperature: 0.2,
            max_output_tokens: Some(max_tokens),
            metadata: RequestMetadata {
                segment_id: Some(format!("batch_{}", batch.id)),
                block_ids: batch.items.iter().map(|i| i.block_id.0.clone()).collect(),
                prompt_template: Some(template.name.clone()),
                prompt_version: Some(template.version.clone()),
                provider: Some(config.provider.clone()),
                model: Some(config.model.clone()),
                source_checksum: None,
                runtime_config_revision,
                provider_max_attempts,
            },
        })
        .await;

    match response {
        Ok(resp) => {
            if resp.finish_reason == FinishReason::Length {
                return Err(LlmError::InvalidResponse(
                    "batch output was truncated: max_output_tokens limit reached".to_string(),
                ));
            }

            let mut result = parse_batch_response_with_validation(
                &batch,
                &resp.content,
                validate_source_copy,
                Some(section_titles),
            )
            .map_err(LlmError::InvalidResponse)?;
            result.input_tokens = resp.input_tokens;
            result.input_cached_tokens = resp.input_cached_tokens;
            result.output_tokens = resp.output_tokens;
            apportion_batch_usage(&batch, &mut result);
            Ok(result)
        }
        Err(e) => Err(e),
    }
}

async fn context_pairs_for_batch(
    batch: &TranslationBatch,
    config: &TranslationRunConfig,
) -> Vec<crate::scheduler::CompletedContext> {
    // Sliding-context fence (ROADMAP §6.4 — PR5 makes this work in batch
    // mode). build_translation_batches guarantees no batch crosses a
    // section boundary, so awaiting the batch's earliest segment is safe:
    // its prior-N dependencies are necessarily in *earlier* batches of
    // the same section (or earlier sections, depending on scope) and
    // can't deadlock on a sibling item in this same batch. In strict mode
    // the scheduler calls this before acquiring request concurrency so
    // context waiters cannot starve prerequisite split batches; in
    // best-effort mode it is called after permits are held and returns
    // without waiting.
    match (config.context_registry.as_deref(), batch.kind) {
        (Some(registry), BatchKind::Translation) if config.context.enabled() => {
            let earliest = batch
                .items
                .iter()
                .min_by_key(|item| item.ordinal)
                .map(|item| item.segment_id.clone());
            match earliest {
                Some(segment_id) => {
                    registry
                        .await_context_for(&segment_id, config.context)
                        .await
                }
                None => Vec::new(),
            }
        }
        _ => Vec::new(),
    }
}

fn render_batch_items(batch: &TranslationBatch, config: &TranslationRunConfig) -> String {
    let items: Vec<serde_json::Value> = batch
        .items
        .iter()
        .map(|item| {
            let mut obj = serde_json::json!({
                "id": item.item_id,
                "kind": item.kind,
                "text": item.source_text,
                "required_markers": item.required_markers,
                "protected": item.protected_spans,
            })
            .as_object()
            .cloned()
            .unwrap_or_default();

            let entries = config
                .glossary
                .entries_by_segment
                .get(&item.segment_id.0)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            match config.glossary.format {
                GlossaryFormat::Json => {
                    obj.insert(
                        "glossary".to_string(),
                        serde_json::to_value(entries)
                            .unwrap_or_else(|_| serde_json::Value::Array(Vec::new())),
                    );
                }
                GlossaryFormat::Prose => {
                    obj.insert(
                        "glossary_prose".to_string(),
                        serde_json::Value::String(crate::scheduler::render_glossary_prose(entries)),
                    );
                }
            }

            if let Some(guidance) = config.glossary.guidance_by_segment.get(&item.segment_id.0) {
                obj.insert(
                    "retry_guidance".to_string(),
                    serde_json::Value::String(guidance.clone()),
                );
            }

            if batch.mode == BatchMode::RunPreserving {
                let runs: Vec<serde_json::Value> = item
                    .text_runs
                    .iter()
                    .map(|r| serde_json::json!({"id": r.id, "text": r.text}))
                    .collect();
                obj.insert("runs".to_string(), serde_json::Value::Array(runs));
            }
            serde_json::Value::Object(obj)
        })
        .collect();

    serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_string())
}

fn apportion_batch_usage(batch: &TranslationBatch, result: &mut BatchTranslationResult) {
    let total_input = result.input_tokens;
    let total_cached = result.input_cached_tokens;
    let total_output = result.output_tokens;
    if total_input.is_none() && total_cached.is_none() && total_output.is_none() {
        return;
    }

    let weights = batch
        .items
        .iter()
        .map(|item| token_estimate(&item.source_text).max(1) as u64)
        .collect::<Vec<_>>();
    if weights.is_empty() {
        return;
    }

    let input = apportion(total_input, &weights);
    let cached = apportion(total_cached, &weights);
    let output = apportion(total_output, &weights);
    let estimated = batch.items.len() > 1;

    let usage_by_item = batch
        .items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            (
                item.item_id.as_str(),
                (input[index], cached[index], output[index]),
            )
        })
        .collect::<HashMap<_, _>>();

    for translation in &mut result.translations {
        if let Some((input, cached, output)) = usage_by_item.get(translation.item_id.as_str()) {
            translation.input_tokens = *input;
            translation.input_cached_tokens = *cached;
            translation.output_tokens = *output;
            translation.tokens_estimated = estimated;
        }
    }

    for failure in &mut result.failures {
        if let Some((input, cached, output)) = usage_by_item.get(failure.item_id.as_str()) {
            failure.input_tokens = *input;
            failure.input_cached_tokens = *cached;
            failure.output_tokens = *output;
            failure.tokens_estimated = estimated;
        }
    }
}

fn add_usage(entry: &mut SegmentTranslation, item: &BatchItemTranslation) {
    entry.input_tokens = add_optional(entry.input_tokens, item.input_tokens);
    entry.input_cached_tokens = add_optional(entry.input_cached_tokens, item.input_cached_tokens);
    entry.output_tokens = add_optional(entry.output_tokens, item.output_tokens);
    entry.tokens_estimated |= item.tokens_estimated;
}

fn add_failure_usage(entry: &mut SegmentTranslation, item: &BatchItemFailure) {
    entry.input_tokens = add_optional(entry.input_tokens, item.input_tokens);
    entry.input_cached_tokens = add_optional(entry.input_cached_tokens, item.input_cached_tokens);
    entry.output_tokens = add_optional(entry.output_tokens, item.output_tokens);
    entry.tokens_estimated |= item.tokens_estimated;
}

fn append_translation_error(entry: &mut SegmentTranslation, error: &str) {
    match entry.error.as_mut() {
        Some(existing) if existing == error => {}
        Some(existing) => {
            existing.push_str("; ");
            existing.push_str(error);
        }
        None => entry.error = Some(error.to_string()),
    }
}

fn repairable_batch_failure(failure: &BatchItemFailure) -> bool {
    !matches!(
        failure.error.as_str(),
        error if error.starts_with("HTTP status ")
            || error.starts_with("HTTP error:")
            || error.starts_with("provider error:")
            || error.contains("semaphore closed")
            || error.contains("concurrency limiter closed")
    )
}

fn order_blocks_by_segment(
    block_ids: &[BlockId],
    blocks: Vec<BlockTranslation>,
) -> (Vec<BlockTranslation>, Vec<String>, Vec<String>, Vec<String>) {
    let mut by_id: HashMap<BlockId, Vec<BlockTranslation>> = HashMap::new();
    for block in blocks {
        by_id.entry(block.block_id.clone()).or_default().push(block);
    }

    let mut ordered = Vec::with_capacity(block_ids.len());
    let mut missing = Vec::new();
    let mut duplicate = Vec::new();
    for block_id in block_ids {
        match by_id.remove(block_id) {
            Some(mut matches) => {
                if matches.len() > 1 {
                    duplicate.push(block_id.0.clone());
                }
                ordered.push(matches.remove(0));
            }
            None => missing.push(block_id.0.clone()),
        }
    }

    let mut extra = by_id.keys().map(|id| id.0.clone()).collect::<Vec<_>>();
    extra.sort();
    for mut extras in by_id.into_values() {
        ordered.append(&mut extras);
    }

    missing.sort();
    duplicate.sort();
    (ordered, missing, extra, duplicate)
}

fn add_optional(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.saturating_add(right)),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

fn apportion(total: Option<u64>, weights: &[u64]) -> Vec<Option<u64>> {
    let Some(total) = total else {
        return vec![None; weights.len()];
    };
    let weight_sum = weights.iter().sum::<u64>().max(1);
    let mut values = Vec::with_capacity(weights.len());
    let mut used = 0_u64;
    for (index, weight) in weights.iter().enumerate() {
        let value = if index + 1 == weights.len() {
            total.saturating_sub(used)
        } else {
            total.saturating_mul(*weight) / weight_sum
        };
        used = used.saturating_add(value);
        values.push(Some(value));
    }
    values
}

fn request_status_from_error(error: &LlmError) -> &'static str {
    match error {
        LlmError::HttpStatus { status: 429, .. } => "rate_limited",
        LlmError::HttpStatus { status, .. } if *status >= 500 => "server_error",
        LlmError::Http(e) if e.is_timeout() => "timeout",
        LlmError::Http(e) if e.is_connect() => "connect_error",
        LlmError::InvalidResponse(msg) if msg.contains("truncated") => "truncated",
        LlmError::InvalidResponse(_) => "invalid_response",
        LlmError::Json(_) => "json_error",
        _ => "error",
    }
}

fn request_status_for_controller<T>(result: &Result<T, LlmError>) -> RequestStatus {
    match result {
        Ok(_) => RequestStatus::Ok,
        Err(LlmError::HttpStatus { status: 429, .. }) => RequestStatus::RateLimited,
        Err(LlmError::HttpStatus { status, .. }) if *status >= 500 => RequestStatus::ServerError,
        Err(LlmError::Http(error)) if error.is_timeout() => RequestStatus::Timeout,
        Err(LlmError::Http(error)) if error.is_connect() => RequestStatus::ConnectError,
        Err(LlmError::InvalidResponse(message)) if message.contains("truncated") => {
            RequestStatus::Truncated
        }
        Err(LlmError::InvalidResponse(_)) | Err(LlmError::Json(_)) => RequestStatus::InvalidJson,
        Err(_) => RequestStatus::OtherError,
    }
}

#[cfg(test)]
mod tests;
