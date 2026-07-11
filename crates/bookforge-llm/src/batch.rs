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
    sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError, mpsc},
    task::JoinSet,
};

use crate::{
    CompletionRequest, FinishReason, LlmError, LlmProvider, PromptLibrary, ProviderRateController,
    RequestMetadata, RequestStatus, ResponseFormat, SegmentTranslation, Substitutions,
    TelemetryLog, TranslationRunConfig,
    concurrency::{PauseSignal, PauseState},
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
    request_permit: Option<OwnedSemaphorePermit>,
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

impl BatchSizer {
    pub fn new(target_tokens: usize, max_items: usize) -> Self {
        Self::new_with_progress(target_tokens, max_items, None)
    }

    pub fn with_progress(
        target_tokens: usize,
        max_items: usize,
        progress: Arc<dyn bookforge_core::ProgressSink>,
    ) -> Self {
        Self::new_with_progress(target_tokens, max_items, Some(progress))
    }

    pub fn new_with_progress(
        target_tokens: usize,
        max_items: usize,
        progress: Option<Arc<dyn bookforge_core::ProgressSink>>,
    ) -> Self {
        let mut modes = HashMap::new();
        for mode in [
            BatchMode::Plain,
            BatchMode::TurboTextOnly,
            BatchMode::MarkerSafe,
            BatchMode::RunPreserving,
        ] {
            modes.insert(
                mode,
                BatchModeSizing::for_mode(mode, target_tokens, max_items),
            );
        }
        Self {
            modes,
            default_target_tokens: target_tokens,
            default_max_items: max_items,
            progress,
        }
    }

    pub fn target_tokens(&self) -> usize {
        self.target_tokens_for_mode(BatchMode::Plain)
    }

    pub fn max_items(&self) -> usize {
        self.max_items_for_mode(BatchMode::Plain)
    }

    pub fn target_tokens_for_mode(&self, mode: BatchMode) -> usize {
        self.modes
            .get(&mode)
            .map(|state| state.target_tokens)
            .unwrap_or_else(|| {
                self.default_target_tokens
                    .clamp(mode.min_tokens(), mode.max_tokens())
            })
    }

    pub fn max_items_for_mode(&self, mode: BatchMode) -> usize {
        self.modes
            .get(&mode)
            .map(|state| state.max_items)
            .unwrap_or_else(|| {
                self.default_max_items
                    .clamp(mode.min_items(), mode.max_items_cap())
            })
    }

    fn emit_change(
        &self,
        reason: &str,
        prev_target: usize,
        new_target: usize,
        prev_max: usize,
        new_max: usize,
    ) {
        if let Some(ref p) = self.progress {
            p.emit(bookforge_core::ProgressEvent::BatchSizingChanged {
                batch_id: None,
                previous_target: prev_target,
                new_target,
                previous_max_items: prev_max,
                new_max_items: new_max,
                reason: reason.to_string(),
                timestamp_ms: bookforge_core::progress::now_ms(),
            });
        }
    }

    pub fn on_truncation(&mut self) {
        self.on_truncation_for_mode(BatchMode::Plain);
    }

    pub fn on_truncation_for_mode(&mut self, mode: BatchMode) {
        if let Some((prev_target, new_target, prev_max, new_max)) =
            self.decrease_mode(mode, BatchSizingObservation::Truncation, 0.65, 0.75)
        {
            self.emit_change("truncation", prev_target, new_target, prev_max, new_max);
        }
    }

    pub fn on_invalid_json(&mut self) {
        self.on_invalid_json_for_mode(BatchMode::Plain);
    }

    pub fn on_invalid_json_for_mode(&mut self, mode: BatchMode) {
        if let Some((prev_target, new_target, prev_max, new_max)) =
            self.decrease_mode(mode, BatchSizingObservation::InvalidJson, 0.75, 0.85)
        {
            self.emit_change("invalid_json", prev_target, new_target, prev_max, new_max);
        }
    }

    pub fn on_p95_high(&mut self) {
        self.on_high_latency_for_mode(BatchMode::Plain, BATCH_SIZER_TARGET_P95_LATENCY_MS + 1);
    }

    pub fn on_high_latency_for_mode(&mut self, mode: BatchMode, latency_ms: u64) {
        if let Some((prev_target, new_target, prev_max, new_max)) = self.decrease_mode(
            mode,
            BatchSizingObservation::HighLatency { latency_ms },
            0.85,
            1.0,
        ) {
            self.emit_change("high_latency", prev_target, new_target, prev_max, new_max);
        }
    }

    pub fn on_success(&mut self) {
        self.on_success_for_mode(BatchMode::Plain, 0);
    }

    pub fn on_success_for_mode(&mut self, mode: BatchMode, latency_ms: u64) {
        let changed = {
            let state = self
                .modes
                .get_mut(&mode)
                .expect("all batch modes initialized");
            state.push_observation(BatchSizingObservation::Success { latency_ms });

            if state
                .p95_latency_ms()
                .is_some_and(|p95| p95 > BATCH_SIZER_TARGET_P95_LATENCY_MS)
            {
                let prev_target = state.target_tokens;
                let prev_max = state.max_items;
                if state.apply_decrease(0.85, 1.0) {
                    Some((
                        "high_latency",
                        prev_target,
                        state.target_tokens,
                        prev_max,
                        state.max_items,
                    ))
                } else {
                    None
                }
            } else if state.should_grow() {
                let prev_target = state.target_tokens;
                let prev_max = state.max_items;
                state.target_tokens = ((state.target_tokens as f64) * 1.10).round() as usize;
                state.max_items = state.max_items.saturating_add(mode.success_item_step());
                state.clamp();
                state.last_increase = Some(Instant::now());
                Some((
                    "stable_success",
                    prev_target,
                    state.target_tokens,
                    prev_max,
                    state.max_items,
                ))
            } else {
                None
            }
        };

        if let Some((reason, prev_target, new_target, prev_max, new_max)) = changed {
            self.emit_change(reason, prev_target, new_target, prev_max, new_max);
        }
    }

    fn decrease_mode(
        &mut self,
        mode: BatchMode,
        observation: BatchSizingObservation,
        target_factor: f64,
        item_factor: f64,
    ) -> Option<(usize, usize, usize, usize)> {
        let state = self
            .modes
            .get_mut(&mode)
            .expect("all batch modes initialized");
        state.push_observation(observation);
        let prev_target = state.target_tokens;
        let prev_max = state.max_items;
        state.apply_decrease(target_factor, item_factor).then_some((
            prev_target,
            state.target_tokens,
            prev_max,
            state.max_items,
        ))
    }
}

impl BatchModeSizing {
    fn for_mode(mode: BatchMode, initial_target_tokens: usize, initial_max_items: usize) -> Self {
        let mut state = Self {
            target_tokens: initial_target_tokens,
            max_items: initial_max_items,
            initial_target_tokens,
            initial_max_items,
            min_tokens: mode.min_tokens(),
            max_tokens: mode.max_tokens(),
            min_items: mode.min_items(),
            max_items_cap: mode.max_items_cap(),
            recent: VecDeque::new(),
            last_increase: None,
            last_decrease: None,
        };
        state.clamp();
        state
    }

    fn clamp(&mut self) {
        self.target_tokens = self.target_tokens.clamp(self.min_tokens, self.max_tokens);
        self.max_items = self.max_items.clamp(self.min_items, self.max_items_cap);
    }

    fn push_observation(&mut self, observation: BatchSizingObservation) {
        self.recent.push_back(observation);
        while self.recent.len() > BATCH_SIZER_WINDOW {
            self.recent.pop_front();
        }
    }

    fn success_rate(&self) -> f64 {
        if self.recent.is_empty() {
            return 0.0;
        }
        let success_count = self
            .recent
            .iter()
            .filter(|obs| matches!(obs, BatchSizingObservation::Success { .. }))
            .count();
        success_count as f64 / self.recent.len() as f64
    }

    fn has_recent_truncation_or_invalid_json(&self) -> bool {
        self.recent.iter().any(|obs| {
            matches!(
                obs,
                BatchSizingObservation::Truncation | BatchSizingObservation::InvalidJson
            )
        })
    }

    fn p95_latency_ms(&self) -> Option<u64> {
        let mut latencies = self
            .recent
            .iter()
            .filter_map(|obs| match obs {
                BatchSizingObservation::Success { latency_ms }
                | BatchSizingObservation::HighLatency { latency_ms } => Some(*latency_ms),
                _ => None,
            })
            .collect::<Vec<_>>();
        if latencies.is_empty() {
            return None;
        }
        latencies.sort_unstable();
        let idx = ((latencies.len() as f64) * 0.95).ceil() as usize;
        Some(latencies[idx.saturating_sub(1).min(latencies.len() - 1)])
    }

    fn should_grow(&self) -> bool {
        if self.recent.len() < BATCH_SIZER_WINDOW {
            return false;
        }
        if self.success_rate() < BATCH_SIZER_STABLE_SUCCESS_THRESHOLD {
            return false;
        }
        if self.has_recent_truncation_or_invalid_json() {
            return false;
        }
        if self
            .p95_latency_ms()
            .is_some_and(|p95| p95 > BATCH_SIZER_TARGET_P95_LATENCY_MS)
        {
            return false;
        }
        self.last_increase
            .map(|last| last.elapsed() >= BATCH_SIZER_INCREASE_INTERVAL)
            .unwrap_or(true)
    }

    fn apply_decrease(&mut self, target_factor: f64, item_factor: f64) -> bool {
        if self
            .last_decrease
            .map(|last| last.elapsed() < BATCH_SIZER_DECREASE_INTERVAL)
            .unwrap_or(false)
        {
            return false;
        }
        let prev_target = self.target_tokens;
        let prev_items = self.max_items;
        self.target_tokens = ((self.target_tokens as f64) * target_factor).floor() as usize;
        self.max_items = ((self.max_items as f64) * item_factor).floor() as usize;
        self.clamp();
        self.last_decrease = Some(Instant::now());
        self.target_tokens != prev_target || self.max_items != prev_items
    }
}

impl BatchMode {
    fn min_tokens(self) -> usize {
        match self {
            Self::Plain | Self::TurboTextOnly => 4_000,
            Self::MarkerSafe => 2_000,
            Self::RunPreserving => 1_000,
        }
    }

    fn max_tokens(self) -> usize {
        match self {
            Self::Plain | Self::TurboTextOnly => 32_000,
            Self::MarkerSafe => 16_000,
            Self::RunPreserving => 8_000,
        }
    }

    fn min_items(self) -> usize {
        match self {
            Self::Plain | Self::TurboTextOnly => 16,
            Self::MarkerSafe => 8,
            Self::RunPreserving => 4,
        }
    }

    fn max_items_cap(self) -> usize {
        match self {
            Self::Plain | Self::TurboTextOnly => 256,
            Self::MarkerSafe => 128,
            Self::RunPreserving => 64,
        }
    }

    fn success_item_step(self) -> usize {
        match self {
            Self::Plain | Self::TurboTextOnly => 16,
            Self::MarkerSafe => 8,
            Self::RunPreserving => 4,
        }
    }
}

pub fn build_translation_batches(
    segments: &[Segment],
    config: &BatchConfig,
    profile: TranslationProfile,
) -> Vec<TranslationBatch> {
    if !config.enabled {
        return Vec::new();
    }

    let turbo = profile == TranslationProfile::TurboTextOnly;

    fn strip_markers(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let chars: Vec<char> = text.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '<'
                && i + 1 < chars.len()
                && let Some(end) = chars[i..].iter().position(|&c| c == '>')
            {
                i += end + 1;
                out.push(' ');
                continue;
            }
            out.push(chars[i]);
            i += 1;
        }
        out
    }

    let mut items: Vec<TranslationBatchItem> = Vec::new();
    let mut ordinal = 0usize;

    for segment in segments {
        for block in &segment.source.blocks {
            let (source_text, required_markers, protected_spans) = if turbo {
                (strip_markers(&block.text), Vec::new(), Vec::new())
            } else {
                (
                    block.text.clone(),
                    bookforge_core::marker::marker_ids_in_text(&block.text),
                    block.protected_spans.clone(),
                )
            };

            items.push(TranslationBatchItem {
                item_id: format!("{}:{}", segment.id.0, block.block_id.0),
                segment_id: segment.id.clone(),
                section_id: segment.section_id.clone(),
                block_id: block.block_id.clone(),
                ordinal,
                kind: block.kind.clone(),
                source_text,
                text_runs: block.text_runs.clone(),
                protected_spans,
                required_markers,
                checksum: segment.checksum.clone(),
            });
            ordinal += 1;
        }
    }

    group_batches(items, config)
}

pub fn account_for_batch_prompt_overhead(
    batches: Vec<TranslationBatch>,
    config: &BatchConfig,
    run_config: &TranslationRunConfig,
) -> Vec<TranslationBatch> {
    let target_tokens = mode_target_tokens(config.target_tokens);
    batches
        .into_iter()
        .flat_map(|batch| {
            let token_limit = target_tokens
                .get(&batch.mode)
                .copied()
                .unwrap_or(config.target_tokens);
            repack_batch_with_config(batch, token_limit, config.max_items, Some(run_config))
        })
        .collect()
}

fn group_batches(items: Vec<TranslationBatchItem>, config: &BatchConfig) -> Vec<TranslationBatch> {
    // Partition items by (section_id, mode) before token-budget packing.
    // Section partitioning is the invariant that lets the sliding-context
    // fence work in batch mode: a batch never crosses a chapter boundary,
    // so awaiting context for the batch's earliest segment can never
    // deadlock on a sibling item in the same batch.
    let mut section_mode_groups: HashMap<
        (bookforge_core::ir::SectionId, BatchMode),
        Vec<TranslationBatchItem>,
    > = HashMap::new();
    for item in items {
        let key = (item.section_id.clone(), item.mode());
        section_mode_groups.entry(key).or_default().push(item);
    }

    // Walk groups in (section ordinal, mode) order so the output `batches`
    // vec ends up ordered as the source document reads. The scheduler relies
    // on this to dispatch earlier sections first.
    let mut ordered_keys: Vec<(bookforge_core::ir::SectionId, BatchMode)> =
        section_mode_groups.keys().cloned().collect();
    ordered_keys.sort_by(|a, b| {
        let section_a = section_mode_groups[a]
            .iter()
            .map(|item| item.ordinal)
            .min()
            .unwrap_or(usize::MAX);
        let section_b = section_mode_groups[b]
            .iter()
            .map(|item| item.ordinal)
            .min()
            .unwrap_or(usize::MAX);
        section_a
            .cmp(&section_b)
            .then_with(|| (a.1 as u8).cmp(&(b.1 as u8)))
    });

    let target_tokens = mode_target_tokens(config.target_tokens);
    let mut batches = Vec::new();
    let mut batch_ordinal = 0usize;

    for key in ordered_keys {
        let (section_id, mode) = key.clone();
        let group_items = section_mode_groups.remove(&key).unwrap_or_default();
        let token_limit = target_tokens
            .get(&mode)
            .copied()
            .unwrap_or(config.target_tokens);
        let max_items = config.max_items;

        let mut current: Vec<TranslationBatchItem> = Vec::new();
        let mut current_tokens = 0usize;

        for item in group_items {
            let item_tokens = token_estimate(&item.source_text);
            let would_exceed_tokens =
                !current.is_empty() && current_tokens + item_tokens > token_limit;
            let would_exceed_items = max_items > 0 && current.len() >= max_items;

            if would_exceed_tokens || would_exceed_items {
                let batch = make_batch(
                    format!("batch_{:04}", batch_ordinal),
                    batch_ordinal,
                    mode,
                    std::mem::take(&mut current),
                    current_tokens,
                    section_id.clone(),
                );
                batches.push(batch);
                batch_ordinal += 1;
                current_tokens = 0;
            }

            current_tokens += item_tokens;
            current.push(item);
        }

        if !current.is_empty() {
            let batch = make_batch(
                format!("batch_{:04}", batch_ordinal),
                batch_ordinal,
                mode,
                current,
                current_tokens,
                section_id.clone(),
            );
            batches.push(batch);
            batch_ordinal += 1;
        }
    }

    batches
}

fn make_batch(
    id: String,
    ordinal: usize,
    mode: BatchMode,
    items: Vec<TranslationBatchItem>,
    token_estimate: usize,
    section_id: bookforge_core::ir::SectionId,
) -> TranslationBatch {
    TranslationBatch {
        id,
        ordinal,
        mode,
        kind: BatchKind::Translation,
        items,
        token_estimate,
        section_id,
    }
}

fn mode_target_tokens(base: usize) -> HashMap<BatchMode, usize> {
    let mut map = HashMap::new();
    map.insert(BatchMode::Plain, base);
    map.insert(BatchMode::MarkerSafe, base.min(10_000));
    map.insert(BatchMode::RunPreserving, base.min(4_000));
    map.insert(BatchMode::TurboTextOnly, base);
    map
}

fn token_estimate(text: &str) -> usize {
    let chars = text.chars().count();
    if chars == 0 {
        return 0;
    }
    (chars / 4).max(1)
}

fn item_token_estimate(
    item: &TranslationBatchItem,
    config: Option<&TranslationRunConfig>,
) -> usize {
    let mut estimate = token_estimate(&item.source_text).max(1);
    let Some(config) = config else {
        return estimate;
    };

    let entries = config
        .glossary
        .entries_by_segment
        .get(&item.segment_id.0)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    if !entries.is_empty() {
        estimate += match config.glossary.format {
            GlossaryFormat::Json => {
                let rendered = serde_json::to_string(entries).unwrap_or_else(|_| "[]".to_string());
                token_estimate("glossary") + token_estimate(&rendered)
            }
            GlossaryFormat::Prose => {
                let rendered = crate::scheduler::render_glossary_prose(entries);
                token_estimate("glossary_prose") + token_estimate(&rendered)
            }
        };
    }
    if let Some(guidance) = config.glossary.guidance_by_segment.get(&item.segment_id.0) {
        estimate += token_estimate("retry_guidance") + token_estimate(guidance);
    }
    estimate
}

fn batch_fixed_token_estimate(config: Option<&TranslationRunConfig>) -> usize {
    config
        .and_then(|config| config.glossary.prompt_extra.as_deref())
        .map(token_estimate)
        .unwrap_or(0)
}

fn batch_token_estimate(
    items: &[TranslationBatchItem],
    config: Option<&TranslationRunConfig>,
) -> usize {
    batch_fixed_token_estimate(config)
        + items
            .iter()
            .map(|item| item_token_estimate(item, config))
            .sum::<usize>()
}

/// Deterministic per-item validation for text-mode batch responses. The
/// markers that must survive are the ones present in THIS block's source;
/// protected spans are already block-scoped.
/// Failures flow into the normal repair/failure pipeline instead of the
/// translation being silently patched up.
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

pub fn split_batch(batch: &TranslationBatch) -> Vec<TranslationBatch> {
    split_batch_with_config(batch, None)
}

fn split_batch_with_config(
    batch: &TranslationBatch,
    config: Option<&TranslationRunConfig>,
) -> Vec<TranslationBatch> {
    if batch.items.len() <= 1 {
        return vec![batch.clone()];
    }
    let mid = batch.items.len() / 2;
    let (left, right) = batch.items.split_at(mid);
    let mut batches = Vec::new();
    if !left.is_empty() {
        batches.push(make_batch(
            format!("{}_split_0", batch.id),
            batch.ordinal * 2,
            batch.mode,
            left.to_vec(),
            batch_token_estimate(left, config),
            batch.section_id.clone(),
        ));
    }
    if !right.is_empty() {
        batches.push(make_batch(
            format!("{}_split_1", batch.id),
            batch.ordinal * 2 + 1,
            batch.mode,
            right.to_vec(),
            batch_token_estimate(right, config),
            batch.section_id.clone(),
        ));
    }
    batches
}

fn normalize_batch_for_current_sizer(
    batch: TranslationBatch,
    sizer: Option<&BatchSizer>,
    config: Option<&TranslationRunConfig>,
) -> Vec<TranslationBatch> {
    let Some(sizer) = sizer else {
        return vec![with_configured_token_estimate(batch, config)];
    };
    let target_tokens = sizer.target_tokens_for_mode(batch.mode);
    let max_items = sizer.max_items_for_mode(batch.mode);
    let batch = with_configured_token_estimate(batch, config);
    if batch.token_estimate <= target_tokens && batch.items.len() <= max_items {
        return vec![batch];
    }
    repack_batch_with_config(batch, target_tokens, max_items, config)
}

#[cfg(test)]
fn repack_batch(
    batch: TranslationBatch,
    target_tokens: usize,
    max_items: usize,
) -> Vec<TranslationBatch> {
    repack_batch_with_config(batch, target_tokens, max_items, None)
}

fn repack_batch_with_config(
    batch: TranslationBatch,
    target_tokens: usize,
    max_items: usize,
    config: Option<&TranslationRunConfig>,
) -> Vec<TranslationBatch> {
    let target_tokens = target_tokens.max(1);
    let max_items = max_items.max(1);
    let mut out = Vec::new();
    let mut current_items = Vec::new();
    let fixed_tokens = batch_fixed_token_estimate(config);
    let mut current_tokens = fixed_tokens;
    let mut part = 0usize;
    let base_id = batch.id;
    let base_ordinal = batch.ordinal;
    let mode = batch.mode;
    let kind = batch.kind;
    let section_id = batch.section_id;

    for item in batch.items {
        let item_tokens = item_token_estimate(&item, config).max(1);
        let would_exceed_items = current_items.len() >= max_items;
        let would_exceed_tokens =
            !current_items.is_empty() && current_tokens + item_tokens > target_tokens;
        if would_exceed_items || would_exceed_tokens {
            out.push(TranslationBatch {
                id: format!("{base_id}_adaptive_{part}"),
                ordinal: base_ordinal * 1000 + part,
                mode,
                kind,
                items: std::mem::take(&mut current_items),
                token_estimate: current_tokens,
                section_id: section_id.clone(),
            });
            current_tokens = fixed_tokens;
            part += 1;
        }
        current_tokens += item_tokens;
        current_items.push(item);
    }

    if !current_items.is_empty() {
        out.push(TranslationBatch {
            id: format!("{base_id}_adaptive_{part}"),
            ordinal: base_ordinal * 1000 + part,
            mode,
            kind,
            items: current_items,
            token_estimate: current_tokens,
            section_id,
        });
    }
    out
}

fn with_configured_token_estimate(
    mut batch: TranslationBatch,
    config: Option<&TranslationRunConfig>,
) -> TranslationBatch {
    batch.token_estimate = batch_token_estimate(&batch.items, config);
    batch
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
    let concurrency = config.scheduler.concurrency.max(1);
    let pause_signal = config.pause_signal.clone();
    let request_semaphore = Arc::new(Semaphore::new(concurrency));

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
    let mut single_invalid_attempts: HashMap<String, usize> = HashMap::new();
    let mut transient_attempts: HashMap<String, usize> = HashMap::new();
    let mut escalated_output_tokens: HashMap<String, u32> = HashMap::new();
    let mut truncation_alert = TruncationAlertState::default();
    let mut stop_dispatch = false;

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
                    if signal.state() == PauseState::Stopped {
                        stop_dispatch = true;
                        break;
                    }
                }

                let Some(batch) = pending_queue.pop_front() else {
                    break;
                };
                let output_override = escalated_output_tokens.remove(&batch.id);
                let mut normalized = normalize_batch_for_current_sizer(
                    batch,
                    batch_sizer.as_deref(),
                    Some(config.as_ref()),
                );
                let batch = normalized.remove(0);
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
                let rate_controller = rate_controller.clone();
                let progress = progress.clone();
                let request_semaphore = request_semaphore.clone();
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
                        match request_semaphore.clone().try_acquire_owned() {
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

                    let permit = match rate_controller.as_ref() {
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

            if let Some(ref controller) = rate_controller {
                controller.observe(request_status, latency_ms);
            }

            match result {
                Ok(batch_result) => {
                    truncation_alert.observe_resolved();
                    if let Some(ref mut sizer) = batch_sizer {
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
                    escalated_output_tokens.insert(batch.id.clone(), next_max_output_tokens);
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
                    if let Some(ref mut sizer) = batch_sizer {
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
                    let attempts = single_invalid_attempts
                        .entry(batch.id.clone())
                        .and_modify(|count| *count += 1)
                        .or_insert(1);
                    if *attempts < config.scheduler.max_attempts.max(1) {
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
                    if let Some(ref mut sizer) = batch_sizer {
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
                    let attempts = transient_attempts
                        .entry(batch.id.clone())
                        .and_modify(|count| *count += 1)
                        .or_insert(1);
                    if *attempts < config.scheduler.max_attempts.max(1) {
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
            while repair_tasks.len() < concurrency {
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
                let config = config.clone();
                let repair_errors = repair_errors.clone();
                let progress = progress.clone();
                let section_titles = section_titles.clone();

                repair_tasks.spawn(async move {
                    let started = std::time::Instant::now();
                    let is_reasoning = provider.is_reasoning();
                    let max_output_tokens =
                        capped_batch_max_output_tokens(&repair_batch, &config, is_reasoning);
                    let request_id = format!("batch_{}", repair_batch.id);

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
                                    metadata: RequestMetadata::default(),
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
mod tests {
    use super::*;
    use bookforge_core::segment::{
        SegmentBlock, SegmentConstraints, SegmentContext, SegmentId, SegmentMetadata,
        SegmentSource, SegmentTextRun,
    };

    fn make_segment(id: &str, blocks: Vec<SegmentBlock>, markers: Vec<String>) -> Segment {
        make_segment_in_section(id, "sec_000000", 0, blocks, markers)
    }

    fn make_segment_in_section(
        id: &str,
        section_id: &str,
        ordinal: usize,
        blocks: Vec<SegmentBlock>,
        markers: Vec<String>,
    ) -> Segment {
        Segment {
            id: SegmentId(id.to_string()),
            section_id: bookforge_core::ir::SectionId(section_id.to_string()),
            ordinal,
            block_ids: blocks.iter().map(|b| b.block_id.clone()).collect(),
            source: SegmentSource {
                text: blocks
                    .iter()
                    .map(|b| b.text.clone())
                    .collect::<Vec<_>>()
                    .join("\n"),
                blocks,
                token_estimate: 50,
            },
            context: SegmentContext::default(),
            metadata: SegmentMetadata::default(),
            constraints: SegmentConstraints {
                preserve_markers: markers,
                ..Default::default()
            },
            checksum: format!("checksum_{id}"),
        }
    }

    fn plain_block(text: &str) -> SegmentBlock {
        SegmentBlock {
            block_id: bookforge_core::ir::BlockId(text.to_string()),
            kind: "paragraph".to_string(),
            text: text.to_string(),
            text_runs: vec![SegmentTextRun {
                id: "r0".to_string(),
                text: text.to_string(),
            }],
            protected_spans: Vec::new(),
        }
    }

    fn protected_block(text: &str, spans: Vec<String>) -> SegmentBlock {
        SegmentBlock {
            block_id: bookforge_core::ir::BlockId(text.to_string()),
            kind: "paragraph".to_string(),
            text: text.to_string(),
            text_runs: vec![SegmentTextRun {
                id: "r0".to_string(),
                text: text.to_string(),
            }],
            protected_spans: spans,
        }
    }

    fn single_item_batch_with_protected_span(span: &str) -> TranslationBatch {
        let seg = make_segment(
            "seg1",
            vec![protected_block("Protected number", vec![span.to_string()])],
            vec![],
        );
        let config = BatchConfig {
            enabled: true,
            target_tokens: 1000,
            max_items: 64,
            adaptive_sizing: false,
            split_on_json_failure: true,
            repair_invalid_items: true,
        };
        build_translation_batches(&[seg], &config, TranslationProfile::Balanced)
            .into_iter()
            .next()
            .expect("single batch")
    }

    fn batch_item(id: &str, source_text: &str) -> TranslationBatchItem {
        TranslationBatchItem {
            item_id: id.to_string(),
            segment_id: SegmentId(format!("seg_{id}")),
            section_id: bookforge_core::ir::SectionId("test_section".to_string()),
            block_id: bookforge_core::ir::BlockId(format!("block_{id}")),
            ordinal: 0,
            kind: "paragraph".to_string(),
            source_text: source_text.to_string(),
            text_runs: Vec::new(),
            protected_spans: Vec::new(),
            required_markers: Vec::new(),
            checksum: format!("checksum_{id}"),
        }
    }

    fn run_preserving_batch_with_runs(run_texts: &[&str]) -> TranslationBatch {
        let mut item = batch_item("runs", &run_texts.join(""));
        item.text_runs = run_texts
            .iter()
            .enumerate()
            .map(|(index, text)| SegmentTextRun {
                id: format!("r{index}"),
                text: (*text).to_string(),
            })
            .collect();
        TranslationBatch {
            id: "run-preserving".to_string(),
            ordinal: 0,
            mode: BatchMode::RunPreserving,
            kind: BatchKind::Translation,
            token_estimate: 100,
            items: vec![item.clone()],
            section_id: item.section_id,
        }
    }

    #[test]
    fn plain_blocks_batch_together() {
        let seg1 = make_segment("seg1", vec![plain_block("Hello world")], vec![]);
        let seg2 = make_segment("seg2", vec![plain_block("Goodbye world")], vec![]);
        let config = BatchConfig {
            enabled: true,
            target_tokens: 1000,
            max_items: 64,
            adaptive_sizing: false,
            split_on_json_failure: true,
            repair_invalid_items: true,
        };
        let batches =
            build_translation_batches(&[seg1, seg2], &config, TranslationProfile::Balanced);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].items.len(), 2);
    }

    #[test]
    fn batch_construction_uses_only_block_local_markers() {
        let seg = make_segment(
            "seg1",
            vec![plain_block("<m1>Marked</m1>"), plain_block("Plain sibling")],
            vec!["m1".to_string()],
        );
        let config = BatchConfig {
            enabled: true,
            target_tokens: 1000,
            max_items: 64,
            adaptive_sizing: false,
            split_on_json_failure: true,
            repair_invalid_items: true,
        };

        let batches = build_translation_batches(&[seg], &config, TranslationProfile::Balanced);
        let items = batches
            .iter()
            .flat_map(|batch| batch.items.iter())
            .collect::<Vec<_>>();
        let marked = items
            .iter()
            .find(|item| item.source_text.contains("Marked"))
            .expect("marked block");
        let plain = items
            .iter()
            .find(|item| item.source_text.contains("Plain sibling"))
            .expect("plain block");

        assert_eq!(marked.required_markers, vec!["m1"]);
        assert!(plain.required_markers.is_empty());
        assert_eq!(plain.mode(), BatchMode::Plain);
    }

    #[test]
    fn batches_never_cross_section_boundaries() {
        // PR5 invariant: build_translation_batches must partition by section
        // before grouping by token budget, so sliding-context awaiting in
        // batch mode can't deadlock on a sibling item in the same batch.
        let seg_a1 =
            make_segment_in_section("a1", "sec_A", 0, vec![plain_block("Alpha one")], vec![]);
        let seg_a2 =
            make_segment_in_section("a2", "sec_A", 1, vec![plain_block("Alpha two")], vec![]);
        let seg_b1 =
            make_segment_in_section("b1", "sec_B", 2, vec![plain_block("Bravo one")], vec![]);
        let seg_b2 =
            make_segment_in_section("b2", "sec_B", 3, vec![plain_block("Bravo two")], vec![]);
        let config = BatchConfig {
            enabled: true,
            target_tokens: 100_000,
            max_items: 64,
            adaptive_sizing: false,
            split_on_json_failure: true,
            repair_invalid_items: true,
        };
        let batches = build_translation_batches(
            &[seg_a1, seg_a2, seg_b1, seg_b2],
            &config,
            TranslationProfile::Balanced,
        );
        // Token budget could fit all four in one batch — but section
        // partitioning forces two batches, one per section.
        assert_eq!(batches.len(), 2);
        for batch in &batches {
            let section_set: std::collections::HashSet<&str> = batch
                .items
                .iter()
                .map(|item| item.section_id.0.as_str())
                .collect();
            assert_eq!(
                section_set.len(),
                1,
                "batch {} mixes sections: {:?}",
                batch.id,
                section_set
            );
            // Batch.section_id matches its items'.
            assert_eq!(
                batch.section_id.0, batch.items[0].section_id.0,
                "batch.section_id must match its items"
            );
        }
    }

    #[test]
    fn batches_emerge_in_input_order_across_sections() {
        // build_translation_batches respects the input order of `segments`
        // (which `build_segments` produces in document order). The dispatcher
        // pulls batches FIFO from the queue, so earlier-input sections get
        // dispatched first.
        let seg_a = make_segment_in_section("a", "sec_A", 0, vec![plain_block("Alpha")], vec![]);
        let seg_b = make_segment_in_section("b", "sec_B", 1, vec![plain_block("Bravo")], vec![]);
        let config = BatchConfig {
            enabled: true,
            target_tokens: 100_000,
            max_items: 64,
            adaptive_sizing: false,
            split_on_json_failure: true,
            repair_invalid_items: true,
        };
        let batches =
            build_translation_batches(&[seg_a, seg_b], &config, TranslationProfile::Balanced);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].section_id.0, "sec_A");
        assert_eq!(batches[1].section_id.0, "sec_B");
    }

    #[test]
    fn batch_sizer_reduces_after_truncation() {
        let mut sizer = BatchSizer::new(16_000, 128);
        sizer.on_truncation_for_mode(BatchMode::Plain);
        assert_eq!(sizer.target_tokens(), 10_400);
        assert_eq!(sizer.max_items(), 96);
    }

    #[test]
    fn batch_sizer_reduces_after_invalid_json() {
        let mut sizer = BatchSizer::new(16_000, 128);
        sizer.on_invalid_json_for_mode(BatchMode::Plain);
        assert_eq!(sizer.target_tokens(), 12_000);
        assert_eq!(sizer.max_items(), 108);
    }

    #[test]
    fn batch_sizer_reduces_after_high_latency() {
        let mut sizer = BatchSizer::new(16_000, 128);
        sizer.on_high_latency_for_mode(BatchMode::Plain, 40_000);
        assert_eq!(sizer.target_tokens(), 13_600);
        assert_eq!(sizer.max_items(), 128);
    }

    #[test]
    fn batch_sizer_increases_after_stable_success() {
        let mut sizer = BatchSizer::new(16_000, 128);
        for _ in 0..20 {
            sizer.on_success_for_mode(BatchMode::Plain, 100);
        }
        assert_eq!(sizer.target_tokens(), 17_600);
        assert_eq!(sizer.max_items(), 144);
    }

    #[test]
    fn batch_sizer_does_not_grow_after_single_success() {
        let mut sizer = BatchSizer::new(16_000, 128);
        sizer.on_success_for_mode(BatchMode::Plain, 100);
        assert_eq!(sizer.target_tokens(), 16_000);
        assert_eq!(sizer.max_items(), 128);
    }

    #[test]
    fn batch_sizer_does_not_grow_when_recent_invalid_json_exists() {
        let mut sizer = BatchSizer::new(16_000, 128);
        sizer.on_invalid_json_for_mode(BatchMode::Plain);
        let after_failure = sizer.target_tokens();
        for _ in 0..19 {
            sizer.on_success_for_mode(BatchMode::Plain, 100);
        }
        assert_eq!(sizer.target_tokens(), after_failure);
    }

    #[test]
    fn batch_sizer_does_not_grow_when_recent_truncation_exists() {
        let mut sizer = BatchSizer::new(16_000, 128);
        sizer.on_truncation_for_mode(BatchMode::Plain);
        let after_failure = sizer.target_tokens();
        for _ in 0..19 {
            sizer.on_success_for_mode(BatchMode::Plain, 100);
        }
        assert_eq!(sizer.target_tokens(), after_failure);
    }

    #[test]
    fn batch_sizer_does_not_grow_when_p95_latency_is_high() {
        let mut sizer = BatchSizer::new(16_000, 128);
        for _ in 0..18 {
            sizer.on_success_for_mode(BatchMode::Plain, 100);
        }
        sizer.on_success_for_mode(BatchMode::Plain, 40_000);
        assert!(sizer.target_tokens() < 16_000);
    }

    #[test]
    fn one_slow_outlier_does_not_immediately_shrink_if_window_p95_healthy() {
        let mut sizer = BatchSizer::new(16_000, 128);
        for _ in 0..19 {
            sizer.on_success_for_mode(BatchMode::Plain, 100);
        }
        sizer.on_success_for_mode(BatchMode::Plain, 40_000);
        assert_eq!(sizer.target_tokens(), 17_600);
    }

    #[test]
    fn batch_sizer_keeps_independent_plain_and_run_preserving_state() {
        let mut sizer = BatchSizer::new(16_000, 128);
        let plain_before = sizer.target_tokens_for_mode(BatchMode::Plain);
        let run_before = sizer.target_tokens_for_mode(BatchMode::RunPreserving);

        sizer.on_invalid_json_for_mode(BatchMode::RunPreserving);

        assert_eq!(sizer.target_tokens_for_mode(BatchMode::Plain), plain_before);
        assert!(sizer.target_tokens_for_mode(BatchMode::RunPreserving) < run_before);
    }

    #[test]
    fn marker_safe_clamp_does_not_affect_turbo_target() {
        let mut sizer = BatchSizer::new(32_000, 256);
        let turbo_before = sizer.target_tokens_for_mode(BatchMode::TurboTextOnly);

        sizer.on_truncation_for_mode(BatchMode::MarkerSafe);

        assert_eq!(
            sizer.target_tokens_for_mode(BatchMode::TurboTextOnly),
            turbo_before
        );
        assert!(sizer.target_tokens_for_mode(BatchMode::MarkerSafe) < 16_000);
    }

    #[test]
    fn batch_sizer_respects_plain_mode_clamps() {
        let sizer = BatchSizer::new(64_000, 512);
        assert_eq!(sizer.target_tokens_for_mode(BatchMode::Plain), 32_000);
        assert_eq!(sizer.max_items_for_mode(BatchMode::Plain), 256);
    }

    #[test]
    fn batch_sizer_respects_marker_safe_clamps() {
        let sizer = BatchSizer::new(64_000, 512);
        assert_eq!(sizer.target_tokens_for_mode(BatchMode::MarkerSafe), 16_000);
        assert_eq!(sizer.max_items_for_mode(BatchMode::MarkerSafe), 128);
    }

    #[test]
    fn batch_sizer_respects_run_preserving_clamps() {
        let sizer = BatchSizer::new(64_000, 512);
        assert_eq!(
            sizer.target_tokens_for_mode(BatchMode::RunPreserving),
            8_000
        );
        assert_eq!(sizer.max_items_for_mode(BatchMode::RunPreserving), 64);
    }

    #[test]
    fn repack_batch_preserves_item_order_and_ids() {
        let batch = TranslationBatch {
            id: "batch".to_string(),
            ordinal: 7,
            mode: BatchMode::Plain,
            kind: BatchKind::Translation,
            token_estimate: 100,
            items: vec![
                batch_item("a", "one two three four"),
                batch_item("b", "five six seven eight"),
                batch_item("c", "nine ten eleven twelve"),
            ],
            section_id: bookforge_core::ir::SectionId("test_section".to_string()),
        };

        let parts = repack_batch(batch, 1, 2);
        let ids = parts
            .iter()
            .flat_map(|part| part.items.iter().map(|item| item.item_id.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["a", "b", "c"]);
        assert!(parts.iter().all(|part| part.items.len() <= 2));
    }

    #[test]
    fn batch_output_budget_accounts_for_many_short_json_items() {
        let items = (0..13)
            .map(|index| batch_item(&format!("item-{index}"), "label"))
            .collect::<Vec<_>>();
        let batch = TranslationBatch {
            id: "short-labels".to_string(),
            ordinal: 0,
            mode: BatchMode::Plain,
            kind: BatchKind::Translation,
            token_estimate: 52,
            items,
            section_id: bookforge_core::ir::SectionId("test_section".to_string()),
        };

        let budget = batch_max_output_tokens(&batch, TranslationProfile::V1Fast, false, false);

        assert!(
            budget >= 1_000,
            "per-item JSON overhead should prevent a 512-token under-budget, got {budget}"
        );
    }

    #[test]
    fn deepseek_batches_can_use_extended_output_budget() {
        let batch = TranslationBatch {
            id: "large".to_string(),
            ordinal: 0,
            mode: BatchMode::RunPreserving,
            kind: BatchKind::Translation,
            token_estimate: 6_000,
            items: (0..30)
                .map(|index| batch_item(&format!("item-{index}"), "longer source text"))
                .collect(),
            section_id: bookforge_core::ir::SectionId("test_section".to_string()),
        };

        assert_eq!(
            batch_max_output_tokens(&batch, TranslationProfile::V1Fast, false, false),
            16_384
        );
        assert!(batch_max_output_tokens(&batch, TranslationProfile::V1Fast, false, true) > 16_384);
    }

    #[test]
    fn batch_sizer_shrink_affects_later_pending_batches() {
        let mut sizer = BatchSizer::new(16_000, 128);
        sizer.on_truncation_for_mode(BatchMode::MarkerSafe);
        let batch = TranslationBatch {
            id: "batch".to_string(),
            ordinal: 0,
            mode: BatchMode::MarkerSafe,
            kind: BatchKind::Translation,
            token_estimate: 80_000,
            items: (0..32)
                .map(|idx| batch_item(&format!("{idx}"), &"word ".repeat(2_000)))
                .collect(),
            section_id: bookforge_core::ir::SectionId("test_section".to_string()),
        };

        let normalized = normalize_batch_for_current_sizer(batch, Some(&sizer), None);
        assert!(normalized.len() > 1);
        assert!(normalized.iter().all(|part| {
            part.token_estimate <= sizer.target_tokens_for_mode(BatchMode::MarkerSafe)
                && part.items.len() <= sizer.max_items_for_mode(BatchMode::MarkerSafe)
        }));
    }

    #[test]
    fn request_status_maps_5xx_to_server_error() {
        let status =
            request_status_for_controller::<BatchTranslationResult>(&Err(LlmError::HttpStatus {
                status: 503,
                body: "unavailable".to_string(),
            }));
        assert_eq!(status, RequestStatus::ServerError);
    }

    #[test]
    fn parses_valid_batch_response() {
        let seg1 = make_segment("seg1", vec![plain_block("Hello")], vec![]);
        let seg2 = make_segment("seg2", vec![plain_block("Goodbye")], vec![]);
        let config = BatchConfig {
            enabled: true,
            target_tokens: 1000,
            max_items: 64,
            adaptive_sizing: false,
            split_on_json_failure: true,
            repair_invalid_items: true,
        };
        let batches =
            build_translation_batches(&[seg1, seg2], &config, TranslationProfile::Balanced);
        let batch = &batches[0];
        let id1 = &batch.items[0].item_id;
        let id2 = &batch.items[1].item_id;

        let response = serde_json::json!({
            "items": [
                {"id": id1, "translation": "Ciao mondo"},
                {"id": id2, "translation": "Addio mondo"},
            ]
        })
        .to_string();

        let result = parse_batch_response(batch, &response).expect("parse");
        assert_eq!(result.translations.len(), 2);
        assert_eq!(result.failures.len(), 0);
    }

    #[test]
    fn missing_protected_span_fails_batch_item_instead_of_appending() {
        let seg = make_segment(
            "seg1",
            vec![protected_block("Chapter 4th", vec!["4th".to_string()])],
            vec!["<bf:keep/>".to_string()],
        );
        let config = BatchConfig {
            enabled: true,
            target_tokens: 1000,
            max_items: 64,
            adaptive_sizing: false,
            split_on_json_failure: true,
            repair_invalid_items: true,
        };
        let batches = build_translation_batches(&[seg], &config, TranslationProfile::Balanced);
        let batch = &batches[0];
        let id = &batch.items[0].item_id;

        let response = serde_json::json!({
            "items": [
                {"id": id, "translation": "Capitolo"},
            ]
        })
        .to_string();

        // The dropped span must surface as an item failure feeding the
        // repair pipeline — never be glued onto the translated text.
        let result = parse_batch_response(batch, &response).expect("parse");
        assert_eq!(result.translations.len(), 0);
        assert_eq!(result.failures.len(), 1);
        assert!(
            result.failures[0]
                .error
                .contains("protected span missing: 4th"),
            "got: {}",
            result.failures[0].error
        );
    }

    #[test]
    fn intact_protected_span_passes_batch_validation_unmodified() {
        let seg = make_segment(
            "seg1",
            vec![protected_block("Chapter 4th", vec!["4th".to_string()])],
            // Segment-wide marker list intentionally names a marker that is
            // NOT in this block's source; per-block validation must not
            // demand it (and must never append it).
            vec!["<bf:keep/>".to_string()],
        );
        let config = BatchConfig {
            enabled: true,
            target_tokens: 1000,
            max_items: 64,
            adaptive_sizing: false,
            split_on_json_failure: true,
            repair_invalid_items: true,
        };
        let batches = build_translation_batches(&[seg], &config, TranslationProfile::Balanced);
        let batch = &batches[0];
        let id = &batch.items[0].item_id;

        let response = serde_json::json!({
            "items": [
                {"id": id, "translation": "Capitolo 4th"},
            ]
        })
        .to_string();

        let result = parse_batch_response(batch, &response).expect("parse");
        assert_eq!(result.failures.len(), 0);
        assert_eq!(result.translations.len(), 1);
        assert_eq!(
            result.translations[0].text, "Capitolo 4th",
            "translation must pass through without appended tokens"
        );
    }

    #[test]
    fn localized_numeric_protected_spans_pass_batch_validation() {
        for (span, translation) in [
            ("0.1", "diametro da 0,1 a 1 mm"),
            ("-63.5", "il potenziale era circa –63,5 mV"),
            ("1957,1989", "Skou (1957, 1989) isolò una ATPasi"),
            ("10-", "7,3 × 10⁻⁷ mol cm⁻²"),
        ] {
            let batch = single_item_batch_with_protected_span(span);
            let id = &batch.items[0].item_id;
            let response = serde_json::json!({
                "items": [
                    {"id": id, "translation": translation},
                ]
            })
            .to_string();

            let result = parse_batch_response(&batch, &response).expect("parse");
            assert_eq!(
                result.failures.len(),
                0,
                "localized numeric form should pass for span {span}"
            );
            assert_eq!(result.translations.len(), 1);
            assert_eq!(result.translations[0].text, translation);
        }
    }

    #[test]
    fn absent_numeric_protected_span_still_fails_batch_validation() {
        let batch = single_item_batch_with_protected_span("5.16");
        let id = &batch.items[0].item_id;
        let response = serde_json::json!({
            "items": [
                {"id": id, "translation": "Si noti che questa forma di rettificazione deriva dai canali aperti."},
            ]
        })
        .to_string();

        let result = parse_batch_response(&batch, &response).expect("parse");
        assert_eq!(result.translations.len(), 0);
        assert_eq!(result.failures.len(), 1);
        assert!(
            result.failures[0]
                .error
                .contains("protected span missing: 5.16"),
            "got: {}",
            result.failures[0].error
        );
    }

    #[test]
    fn missing_marker_close_fails_batch_item_validation() {
        let mut item = batch_item("marked", "<m1>source</m1>");
        item.required_markers = vec!["m1".to_string()];

        let error = batch_item_validation_error(&item, "<m1>translated", false, None)
            .expect("missing marker close should fail");

        assert!(error.contains("missing closing tag"), "got: {error}");
    }

    #[test]
    fn copied_source_prose_fails_batch_item_validation() {
        let source = "This deliberately long English paragraph contains enough ordinary prose to \
            exercise untranslated-copy detection in a real batch response. The provider returned \
            the entire source paragraph unchanged instead of translating it into the requested \
            target language, so this item must enter the normal retry and review pipeline.";
        let item = batch_item("copied", source);

        let error = batch_item_validation_error(&item, source, true, Some("Chapter 1"))
            .expect("long unchanged source prose should fail");

        assert!(error.contains("unchanged from the source-language prose"));
    }

    #[test]
    fn copied_source_prose_fails_internal_batch_response_validation() {
        let source = "This deliberately long English paragraph contains enough ordinary prose to \
            exercise untranslated-copy detection in a real batch response. The provider returned \
            the entire source paragraph unchanged instead of translating it into the requested \
            target language, so this item must enter the normal retry and review pipeline.";
        let item = batch_item("copied-response", source);
        let response = serde_json::json!({
            "items": [{
                "id": item.item_id,
                "translation": source,
            }]
        })
        .to_string();
        let batch = TranslationBatch {
            id: "copied-response".to_string(),
            ordinal: 0,
            mode: BatchMode::Plain,
            kind: BatchKind::Translation,
            token_estimate: 100,
            section_id: item.section_id.clone(),
            items: vec![item.clone()],
        };
        let section_titles = HashMap::from([(item.segment_id.0.clone(), "Chapter 1".to_string())]);

        let result =
            parse_batch_response_with_validation(&batch, &response, true, Some(&section_titles))
                .expect("valid JSON should parse");

        assert!(result.translations.is_empty());
        assert_eq!(result.failures.len(), 1);
        assert!(
            result.failures[0]
                .error
                .contains("unchanged from the source-language prose")
        );
    }

    #[test]
    fn run_preserving_batch_rejects_unknown_run_id_without_success() {
        let batch = run_preserving_batch_with_runs(&["Hello ", "world"]);
        let item = &batch.items[0];
        let response = serde_json::json!({
            "items": [{
                "id": item.item_id,
                "runs": [
                    {"id": "r0", "text": "Ciao "},
                    {"id": "unknown", "text": "mondo"},
                ],
            }]
        })
        .to_string();

        let result = parse_batch_response(&batch, &response).expect("parse");

        assert_eq!(result.translations.len(), 0);
        assert_eq!(result.failures.len(), 1);
        assert!(result.failures[0].error.contains("unknown run ID"));
    }

    #[test]
    fn run_preserving_batch_rejects_duplicate_run_id_without_success() {
        let batch = run_preserving_batch_with_runs(&["Hello ", "world"]);
        let item = &batch.items[0];
        let response = serde_json::json!({
            "items": [{
                "id": item.item_id,
                "runs": [
                    {"id": "r0", "text": "Ciao "},
                    {"id": "r0", "text": "mondo"},
                ],
            }]
        })
        .to_string();

        let result = parse_batch_response(&batch, &response).expect("parse");

        assert_eq!(result.translations.len(), 0);
        assert_eq!(result.failures.len(), 1);
        assert!(result.failures[0].error.contains("duplicate run ID"));
    }

    #[test]
    fn run_preserving_batch_joins_in_source_run_order() {
        let batch = run_preserving_batch_with_runs(&["Hello ", "world"]);
        let item = &batch.items[0];
        let response = serde_json::json!({
            "items": [{
                "id": item.item_id,
                "runs": [
                    {"id": "r1", "text": "mondo"},
                    {"id": "r0", "text": "Ciao "},
                ],
            }]
        })
        .to_string();

        let result = parse_batch_response(&batch, &response).expect("parse");

        assert_eq!(result.failures.len(), 0);
        assert_eq!(result.translations.len(), 1);
        assert_eq!(result.translations[0].text, "Ciao mondo");
    }

    #[test]
    fn run_preserving_batch_rejects_malformed_joined_marker_structure() {
        let mut item = batch_item("marked-runs", "<m1>source</m1>");
        item.text_runs = (0..13)
            .map(|index| SegmentTextRun {
                id: format!("r{index}"),
                text: String::new(),
            })
            .collect();
        item.required_markers = vec!["m1".to_string()];
        let batch = TranslationBatch {
            id: "run-preserving".to_string(),
            ordinal: 0,
            mode: BatchMode::RunPreserving,
            kind: BatchKind::Translation,
            token_estimate: 100,
            items: vec![item.clone()],
            section_id: item.section_id.clone(),
        };
        let runs = (0..13)
            .map(|index| {
                serde_json::json!({
                    "id": format!("r{index}"),
                    "text": if index == 0 { "<m1>translated" } else { "" },
                })
            })
            .collect::<Vec<_>>();
        let response = serde_json::json!({
            "items": [{
                "id": item.item_id,
                "runs": runs,
            }]
        })
        .to_string();

        let result = parse_batch_response(&batch, &response).expect("parse");

        assert_eq!(result.translations.len(), 0);
        assert_eq!(result.failures.len(), 1);
        assert!(
            result.failures[0].error.contains("missing closing tag"),
            "got: {}",
            result.failures[0].error
        );
    }

    #[test]
    fn detects_missing_items_in_batch_response() {
        let seg1 = make_segment("seg1", vec![plain_block("Hello")], vec![]);
        let seg2 = make_segment("seg2", vec![plain_block("Goodbye")], vec![]);
        let config = BatchConfig {
            enabled: true,
            target_tokens: 1000,
            max_items: 64,
            adaptive_sizing: false,
            split_on_json_failure: true,
            repair_invalid_items: true,
        };
        let batches =
            build_translation_batches(&[seg1, seg2], &config, TranslationProfile::Balanced);
        let batch = &batches[0];
        let id1 = &batch.items[0].item_id;

        let response = serde_json::json!({
            "items": [
                {"id": id1, "translation": "Ciao mondo"},
            ]
        })
        .to_string();

        let result = parse_batch_response(batch, &response).expect("parse");
        assert_eq!(result.translations.len(), 1);
        assert_eq!(result.failures.len(), 1);
        assert!(result.failures[0].error.contains("missing"));
    }

    #[test]
    fn detects_duplicate_ids_in_batch_response() {
        let seg1 = make_segment("seg1", vec![plain_block("Hello")], vec![]);
        let config = BatchConfig {
            enabled: true,
            target_tokens: 1000,
            max_items: 64,
            adaptive_sizing: false,
            split_on_json_failure: true,
            repair_invalid_items: true,
        };
        let batches = build_translation_batches(&[seg1], &config, TranslationProfile::Balanced);
        let batch = &batches[0];
        let id1 = &batch.items[0].item_id;

        let response = serde_json::json!({
            "items": [
                {"id": id1, "translation": "Ciao mondo"},
                {"id": id1, "translation": "Duplicato"},
            ]
        })
        .to_string();

        let result = parse_batch_response(batch, &response).expect("parse");
        assert_eq!(result.translations.len(), 1);
        assert_eq!(result.failures.len(), 1);
        assert!(result.failures[0].error.contains("duplicate"));
    }

    #[test]
    fn splits_batch_in_half() {
        let seg1 = make_segment("seg1", vec![plain_block("A")], vec![]);
        let seg2 = make_segment("seg2", vec![plain_block("B")], vec![]);
        let seg3 = make_segment("seg3", vec![plain_block("C")], vec![]);
        let seg4 = make_segment("seg4", vec![plain_block("D")], vec![]);
        let config = BatchConfig {
            enabled: true,
            target_tokens: 1000,
            max_items: 64,
            adaptive_sizing: false,
            split_on_json_failure: true,
            repair_invalid_items: true,
        };
        let batches = build_translation_batches(
            &[seg1, seg2, seg3, seg4],
            &config,
            TranslationProfile::Balanced,
        );
        let split = split_batch(&batches[0]);
        assert_eq!(split.len(), 2);
        assert_eq!(split[0].items.len(), 2);
        assert_eq!(split[1].items.len(), 2);
    }

    use crate::provider::{
        CompletionRequest, CompletionResponse, LlmProvider as LlmProviderTrait,
        ProviderCapabilities, Result as ProviderResult,
    };
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    enum StubBehavior {
        FinishLength,
        ErrInvalid(String),
        ItemsFromBatch(Vec<(String, String)>),
    }

    struct StubProvider {
        behavior: Mutex<Option<StubBehavior>>,
    }

    impl StubProvider {
        fn new(behavior: StubBehavior) -> Self {
            Self {
                behavior: Mutex::new(Some(behavior)),
            }
        }
    }

    enum RecordedResponse {
        FinishLength,
        ItemsFromBatch(Vec<(String, String)>),
    }

    struct RecordingSequenceProvider {
        responses: Mutex<Vec<RecordedResponse>>,
        max_output_tokens: Arc<Mutex<Vec<Option<u32>>>>,
    }

    impl RecordingSequenceProvider {
        fn new(
            responses: Vec<RecordedResponse>,
            max_output_tokens: Arc<Mutex<Vec<Option<u32>>>>,
        ) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().rev().collect()),
                max_output_tokens,
            }
        }
    }

    impl LlmProviderTrait for RecordingSequenceProvider {
        async fn complete(&self, request: CompletionRequest) -> ProviderResult<CompletionResponse> {
            self.max_output_tokens
                .lock()
                .unwrap()
                .push(request.max_output_tokens);
            let response = self
                .responses
                .lock()
                .unwrap()
                .pop()
                .unwrap_or(RecordedResponse::FinishLength);
            match response {
                RecordedResponse::FinishLength => Ok(CompletionResponse {
                    content: "{\"items\":[]}".to_string(),
                    input_tokens: Some(1),
                    input_cached_tokens: Some(0),
                    output_tokens: Some(1),
                    finish_reason: FinishReason::Length,
                    provider_latency_ms: 0,
                    raw: serde_json::json!({}),
                }),
                RecordedResponse::ItemsFromBatch(items) => {
                    let json = serde_json::json!({
                        "items": items
                            .into_iter()
                            .map(|(id, t)| serde_json::json!({"id": id, "translation": t}))
                            .collect::<Vec<_>>(),
                    });
                    Ok(CompletionResponse {
                        content: json.to_string(),
                        input_tokens: Some(1),
                        input_cached_tokens: Some(0),
                        output_tokens: Some(1),
                        finish_reason: FinishReason::Stop,
                        provider_latency_ms: 0,
                        raw: serde_json::json!({}),
                    })
                }
            }
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                supports_json_response_format: true,
                supports_usage_tokens: true,
            }
        }
    }

    struct RecordingProgress {
        events: Arc<Mutex<Vec<bookforge_core::ProgressEvent>>>,
    }

    impl bookforge_core::ProgressSink for RecordingProgress {
        fn emit(&self, event: bookforge_core::ProgressEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    impl LlmProviderTrait for StubProvider {
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> ProviderResult<CompletionResponse> {
            let behavior = self
                .behavior
                .lock()
                .unwrap()
                .take()
                .expect("stub used twice");
            match behavior {
                StubBehavior::FinishLength => Ok(CompletionResponse {
                    content: "{\"items\":[]}".to_string(),
                    input_tokens: Some(1),
                    input_cached_tokens: Some(0),
                    output_tokens: Some(1),
                    finish_reason: FinishReason::Length,
                    provider_latency_ms: 0,
                    raw: serde_json::json!({}),
                }),
                StubBehavior::ErrInvalid(msg) => Err(LlmError::InvalidResponse(msg)),
                StubBehavior::ItemsFromBatch(items) => {
                    let json = serde_json::json!({
                        "items": items
                            .into_iter()
                            .map(|(id, t)| serde_json::json!({"id": id, "translation": t}))
                            .collect::<Vec<_>>(),
                    });
                    Ok(CompletionResponse {
                        content: json.to_string(),
                        input_tokens: Some(1),
                        input_cached_tokens: Some(0),
                        output_tokens: Some(1),
                        finish_reason: FinishReason::Stop,
                        provider_latency_ms: 0,
                        raw: serde_json::json!({}),
                    })
                }
            }
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                supports_json_response_format: true,
                supports_usage_tokens: true,
            }
        }
    }

    fn test_run_config() -> TranslationRunConfig {
        TranslationRunConfig {
            source_language: Some("English".to_string()),
            target_language: "Italian".to_string(),
            provider: "stub".to_string(),
            model: "stub".to_string(),
            prompt_version: "v1".to_string(),
            temperature: 0.2,
            scheduler: bookforge_core::scheduler::SchedulerConfig::default(),
            profile: TranslationProfile::Balanced,
            model_context_tokens: None,
            max_output_tokens: None,
            batch_max_output_tokens: None,
            compact_prompts: false,
            glossary: crate::GlossaryRunConfig::default(),
            context: crate::ContextRunConfig::default(),
            context_registry: None,
            style: None,
            entities: None,
            pause_signal: None,
        }
    }

    fn make_two_item_batch() -> TranslationBatch {
        let seg1 = make_segment("seg1", vec![plain_block("Hello")], vec![]);
        let seg2 = make_segment("seg2", vec![plain_block("Goodbye")], vec![]);
        let config = BatchConfig {
            enabled: true,
            target_tokens: 1000,
            max_items: 64,
            adaptive_sizing: false,
            split_on_json_failure: true,
            repair_invalid_items: true,
        };
        build_translation_batches(&[seg1, seg2], &config, TranslationProfile::Balanced)
            .into_iter()
            .next()
            .unwrap()
    }

    #[test]
    fn batch_items_include_segment_glossary() {
        let batch = make_two_item_batch();
        let mut config = test_run_config();
        config.glossary.entries_by_segment.insert(
            "seg1".to_string(),
            vec![bookforge_core::GlossaryPromptTerm {
                source: "Hello".to_string(),
                target: "Ciao".to_string(),
                category: bookforge_core::GlossaryCategory::Phrase,
                note: None,
                term_id: Some(7),
                case_sensitive: false,
            }],
        );
        config.glossary.prompt_extra = Some("Use informal register.".to_string());
        config.glossary.guidance_by_segment.insert(
            "seg1".to_string(),
            "Translate the greeting less literally.".to_string(),
        );

        let rendered = render_batch_items(&batch, &config);
        assert!(rendered.contains("\"glossary\""));
        assert!(rendered.contains("\"retry_guidance\""));
        assert!(rendered.contains("Translate the greeting less literally."));
        assert!(rendered.contains("\"source\":\"Hello\""));
        assert!(!rendered.contains("Use informal register."));
    }

    #[test]
    fn batch_prompt_overhead_repacks_glossary_heavy_items() {
        let seg1 = make_segment("seg1", vec![plain_block("Hello")], vec![]);
        let seg2 = make_segment("seg2", vec![plain_block("Goodbye")], vec![]);
        let batch_config = BatchConfig {
            enabled: true,
            target_tokens: 120,
            max_items: 64,
            adaptive_sizing: false,
            split_on_json_failure: true,
            repair_invalid_items: true,
        };
        let batches =
            build_translation_batches(&[seg1, seg2], &batch_config, TranslationProfile::Balanced);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].items.len(), 2);

        let mut config = test_run_config();
        for segment_id in ["seg1", "seg2"] {
            config.glossary.entries_by_segment.insert(
                segment_id.to_string(),
                vec![bookforge_core::GlossaryPromptTerm {
                    source: format!("{segment_id}_source"),
                    target: format!("{segment_id}_target"),
                    category: bookforge_core::GlossaryCategory::Phrase,
                    note: Some("x".repeat(480)),
                    term_id: None,
                    case_sensitive: false,
                }],
            );
        }
        config.glossary.prompt_extra = Some("y".repeat(160));

        let adjusted = account_for_batch_prompt_overhead(batches, &batch_config, &config);

        assert_eq!(adjusted.len(), 2);
        assert!(adjusted.iter().all(|batch| batch.items.len() == 1));
        assert!(adjusted.iter().all(|batch| batch.token_estimate > 120));
    }

    #[tokio::test]
    async fn batch_length_finish_reason_returns_invalid_response() {
        let batch = make_two_item_batch();
        let provider = Arc::new(StubProvider::new(StubBehavior::FinishLength));
        let library = Arc::new(PromptLibrary::global().clone());
        let config = test_run_config();

        let result = translate_one_batch(
            provider,
            library,
            batch,
            &config,
            None,
            Vec::new(),
            false,
            &HashMap::new(),
        )
        .await;
        match result {
            Err(LlmError::InvalidResponse(msg)) => {
                assert!(msg.contains("truncated"), "unexpected msg: {msg}")
            }
            other => panic!("expected InvalidResponse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn batch_truncated_error_is_not_swallowed() {
        let batch = make_two_item_batch();
        let provider = Arc::new(StubProvider::new(StubBehavior::ErrInvalid(
            "output was truncated".to_string(),
        )));
        let library = Arc::new(PromptLibrary::global().clone());
        let config = test_run_config();

        let result = translate_one_batch(
            provider,
            library,
            batch,
            &config,
            None,
            Vec::new(),
            false,
            &HashMap::new(),
        )
        .await;
        match result {
            Err(LlmError::InvalidResponse(msg)) => {
                assert!(msg.contains("truncated"), "unexpected msg: {msg}")
            }
            Ok(_) => panic!("truncated error must not be swallowed into Ok"),
            other => panic!("expected InvalidResponse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn batch_truncation_retries_same_batch_with_escalated_budget() {
        let seg1 = make_segment("seg1", vec![plain_block("Hello")], vec![]);
        let seg2 = make_segment("seg2", vec![plain_block("Goodbye")], vec![]);
        let segments = vec![seg1, seg2];
        let cfg = BatchConfig {
            enabled: true,
            target_tokens: 1000,
            max_items: 64,
            adaptive_sizing: false,
            split_on_json_failure: true,
            repair_invalid_items: true,
        };
        let batches = build_translation_batches(&segments, &cfg, TranslationProfile::Balanced);
        let item_ids = batches[0]
            .items
            .iter()
            .map(|item| {
                (
                    item.item_id.clone(),
                    format!("Tradotto {}", item.source_text),
                )
            })
            .collect::<Vec<_>>();
        let max_tokens = Arc::new(Mutex::new(Vec::new()));
        let provider = RecordingSequenceProvider::new(
            vec![
                RecordedResponse::FinishLength,
                RecordedResponse::ItemsFromBatch(item_ids),
            ],
            max_tokens.clone(),
        );
        let events = Arc::new(Mutex::new(Vec::new()));
        let progress = Arc::new(RecordingProgress {
            events: events.clone(),
        });

        let translations = translate_batches_with_callback(
            provider,
            batches,
            &segments,
            &test_run_config(),
            Arc::new(TelemetryLog::new()),
            None,
            None,
            progress,
            None,
            |_| Ok(()),
        )
        .await
        .expect("escalated retry should succeed");

        assert_eq!(translations.len(), 2);
        let budgets = max_tokens.lock().unwrap().clone();
        assert_eq!(budgets.len(), 2);
        assert!(
            budgets[1].unwrap() > budgets[0].unwrap(),
            "second request should use escalated output budget: {budgets:?}"
        );
        let events = events.lock().unwrap();
        assert!(events.iter().any(|event| {
            matches!(
                event,
                bookforge_core::ProgressEvent::Warning { kind, .. }
                    if kind == "batch_truncation_escalated_retry"
            )
        }));
        assert!(
            !events
                .iter()
                .any(|event| { matches!(event, bookforge_core::ProgressEvent::BatchSplit { .. }) })
        );
    }

    #[tokio::test]
    async fn batch_truncation_escalated_retry_survives_adaptive_renaming() {
        let long_source = "long ".repeat(3_600);
        let seg = make_segment("seg1", vec![plain_block(&long_source)], vec![]);
        let segments = vec![seg];
        let cfg = BatchConfig {
            enabled: true,
            target_tokens: 1000,
            max_items: 64,
            adaptive_sizing: true,
            split_on_json_failure: true,
            repair_invalid_items: true,
        };
        let batches = build_translation_batches(&segments, &cfg, TranslationProfile::Balanced);
        assert_eq!(batches.len(), 1);
        assert!(
            batches[0].token_estimate
                > BatchSizer::new(cfg.target_tokens, cfg.max_items).target_tokens(),
            "fixture must force adaptive normalization to rename the single-item batch"
        );
        let item_ids = batches[0]
            .items
            .iter()
            .map(|item| (item.item_id.clone(), "Tradotto lungo".to_string()))
            .collect::<Vec<_>>();
        let max_tokens = Arc::new(Mutex::new(Vec::new()));
        let provider = RecordingSequenceProvider::new(
            vec![
                RecordedResponse::FinishLength,
                RecordedResponse::ItemsFromBatch(item_ids),
            ],
            max_tokens.clone(),
        );
        let events = Arc::new(Mutex::new(Vec::new()));
        let progress = Arc::new(RecordingProgress {
            events: events.clone(),
        });
        let mut sizer = BatchSizer::new(cfg.target_tokens, cfg.max_items);

        let translations = translate_batches_with_callback(
            provider,
            batches,
            &segments,
            &test_run_config(),
            Arc::new(TelemetryLog::new()),
            None,
            Some(&mut sizer),
            progress,
            None,
            |_| Ok(()),
        )
        .await
        .expect("escalated retry should survive adaptive renaming");

        assert_eq!(translations.len(), 1);
        let budgets = max_tokens.lock().unwrap().clone();
        assert_eq!(budgets.len(), 2);
        assert!(
            budgets[1].unwrap() > budgets[0].unwrap(),
            "second request should keep the escalated output budget after adaptive renaming: {budgets:?}"
        );
        let events = events.lock().unwrap();
        assert!(
            !events
                .iter()
                .any(|event| { matches!(event, bookforge_core::ProgressEvent::BatchSplit { .. }) }),
            "batch should not split before the escalated retry"
        );
    }

    #[tokio::test]
    async fn systemic_truncation_emits_alert_after_escalated_failures() {
        let segments = (0..6)
            .map(|idx| make_segment(&format!("seg{idx}"), vec![plain_block("Hello")], vec![]))
            .collect::<Vec<_>>();
        let cfg = BatchConfig {
            enabled: true,
            target_tokens: 1000,
            max_items: 2,
            adaptive_sizing: false,
            split_on_json_failure: true,
            repair_invalid_items: true,
        };
        let batches = build_translation_batches(&segments, &cfg, TranslationProfile::Balanced);
        assert!(
            batches.len() >= 3,
            "fixture should build at least 3 batches"
        );
        let max_tokens = Arc::new(Mutex::new(Vec::new()));
        let provider = RecordingSequenceProvider::new(
            std::iter::repeat_with(|| RecordedResponse::FinishLength)
                .take(64)
                .collect(),
            max_tokens,
        );
        let events = Arc::new(Mutex::new(Vec::new()));
        let progress = Arc::new(RecordingProgress {
            events: events.clone(),
        });

        let _translations = translate_batches_with_callback(
            provider,
            batches,
            &segments,
            &test_run_config(),
            Arc::new(TelemetryLog::new()),
            None,
            None,
            progress,
            None,
            |_| Ok(()),
        )
        .await
        .expect("systemic truncation should become bounded failures");

        let events = events.lock().unwrap();
        assert!(
            events.iter().any(|event| {
                matches!(
                    event,
                    bookforge_core::ProgressEvent::Warning { kind, message, .. }
                        if kind == "systemic_truncation"
                            && message.contains("--batch-max-output-tokens")
                            && message.contains("--batch-max-items")
                )
            }),
            "systemic truncation alert should be emitted"
        );
    }

    #[tokio::test]
    async fn batch_translation_preserves_original_block_ids() {
        let seg1 = make_segment("seg1", vec![plain_block("Hello")], vec![]);
        let seg2 = make_segment("seg2", vec![plain_block("Goodbye")], vec![]);
        let segments = vec![seg1.clone(), seg2.clone()];
        let cfg = BatchConfig {
            enabled: true,
            target_tokens: 1000,
            max_items: 64,
            adaptive_sizing: false,
            split_on_json_failure: true,
            repair_invalid_items: true,
        };
        let batches = build_translation_batches(&segments, &cfg, TranslationProfile::Balanced);
        assert_eq!(batches.len(), 1);
        let item_ids: Vec<(String, String)> = batches[0]
            .items
            .iter()
            .map(|i| (i.item_id.clone(), format!("[it] {}", i.source_text)))
            .collect();

        let provider = StubProvider::new(StubBehavior::ItemsFromBatch(item_ids));
        let telemetry = Arc::new(TelemetryLog::new());
        let config = test_run_config();
        let translations = translate_batches_with_callback(
            provider,
            batches,
            &segments,
            &config,
            telemetry,
            None,
            None,
            Arc::new(bookforge_core::NullProgressSink),
            None,
            |_| Ok(()),
        )
        .await
        .expect("translate");

        assert_eq!(translations.len(), 2);
        for translation in translations {
            for block in &translation.blocks {
                assert!(
                    !block.block_id.0.contains(':'),
                    "block_id leaked compound item id: {}",
                    block.block_id.0,
                );
            }
        }
    }

    struct SequenceProvider {
        responses: Mutex<Vec<String>>,
    }

    impl SequenceProvider {
        fn new(responses: Vec<String>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().rev().collect()),
            }
        }
    }

    impl LlmProviderTrait for SequenceProvider {
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> ProviderResult<CompletionResponse> {
            let next = self
                .responses
                .lock()
                .unwrap()
                .pop()
                .expect("SequenceProvider ran out of responses");
            Ok(CompletionResponse {
                content: next,
                input_tokens: Some(1),
                input_cached_tokens: Some(0),
                output_tokens: Some(1),
                finish_reason: FinishReason::Stop,
                provider_latency_ms: 0,
                raw: serde_json::json!({}),
            })
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                supports_json_response_format: true,
                supports_usage_tokens: true,
            }
        }
    }

    struct FirstInvalidThenPromptEchoProvider {
        calls: Mutex<usize>,
    }

    impl FirstInvalidThenPromptEchoProvider {
        fn new() -> Self {
            Self {
                calls: Mutex::new(0),
            }
        }
    }

    impl LlmProviderTrait for FirstInvalidThenPromptEchoProvider {
        async fn complete(&self, request: CompletionRequest) -> ProviderResult<CompletionResponse> {
            let mut calls = self.calls.lock().unwrap();
            let call = *calls;
            *calls += 1;
            drop(calls);

            let content = if call == 0 {
                "{not valid json".to_string()
            } else {
                let item_ids = item_ids_from_batch_prompt(&request.user);
                serde_json::json!({
                    "items": item_ids
                        .into_iter()
                        .map(|id| serde_json::json!({
                            "id": id,
                            "translation": format!("[it] {id}"),
                        }))
                        .collect::<Vec<_>>(),
                })
                .to_string()
            };

            Ok(CompletionResponse {
                content,
                input_tokens: Some(1),
                input_cached_tokens: Some(0),
                output_tokens: Some(1),
                finish_reason: FinishReason::Stop,
                provider_latency_ms: 0,
                raw: serde_json::json!({}),
            })
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                supports_json_response_format: true,
                supports_usage_tokens: true,
            }
        }
    }

    struct AlwaysTransientProvider {
        calls: AtomicUsize,
    }

    impl AlwaysTransientProvider {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl LlmProviderTrait for Arc<AlwaysTransientProvider> {
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> ProviderResult<CompletionResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(LlmError::HttpStatus {
                status: 503,
                body: "unavailable".to_string(),
            })
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                supports_json_response_format: true,
                supports_usage_tokens: true,
            }
        }
    }

    struct DelayedPromptEchoProvider;

    impl LlmProviderTrait for DelayedPromptEchoProvider {
        async fn complete(&self, request: CompletionRequest) -> ProviderResult<CompletionResponse> {
            let item_ids = item_ids_from_batch_prompt(&request.user);
            if item_ids.iter().any(|id| id.contains("First")) {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            let json = serde_json::json!({
                "items": item_ids
                    .into_iter()
                    .map(|id| {
                        let text = if id.contains("First") {
                            "[it] First"
                        } else if id.contains("Second") {
                            "[it] Second"
                        } else {
                            "[it] Unknown"
                        };
                        serde_json::json!({"id": id, "translation": text})
                    })
                    .collect::<Vec<_>>(),
            });
            Ok(CompletionResponse {
                content: json.to_string(),
                input_tokens: Some(1),
                input_cached_tokens: Some(0),
                output_tokens: Some(1),
                finish_reason: FinishReason::Stop,
                provider_latency_ms: 0,
                raw: serde_json::json!({}),
            })
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                supports_json_response_format: true,
                supports_usage_tokens: true,
            }
        }
    }

    fn item_ids_from_batch_prompt(user_prompt: &str) -> Vec<String> {
        let Some(after_input) = user_prompt.split("Input:\n").nth(1) else {
            return Vec::new();
        };
        let json_text = after_input
            .split("\n\nReturn JSON only.")
            .next()
            .unwrap_or(after_input)
            .trim();
        let Ok(items) = serde_json::from_str::<Vec<serde_json::Value>>(json_text) else {
            return Vec::new();
        };
        items
            .into_iter()
            .filter_map(|item| item.get("id")?.as_str().map(ToString::to_string))
            .collect()
    }

    #[tokio::test]
    async fn partial_batch_failure_without_successful_repair_marks_segment_needs_review() {
        let seg = make_segment(
            "seg1",
            vec![plain_block("Hello"), plain_block("World")],
            vec![],
        );
        let segments = vec![seg.clone()];
        let cfg = BatchConfig {
            enabled: true,
            target_tokens: 1000,
            max_items: 64,
            adaptive_sizing: false,
            split_on_json_failure: true,
            repair_invalid_items: true,
        };
        let batches = build_translation_batches(&segments, &cfg, TranslationProfile::Balanced);
        assert_eq!(batches.len(), 1);
        let first_item_id = batches[0].items[0].item_id.clone();
        let missing_block_id = batches[0].items[1].block_id.0.clone();

        let initial_response = serde_json::json!({
            "items": [
                {"id": first_item_id, "translation": "[it] Hello"},
            ]
        })
        .to_string();
        // Repair returns malformed JSON so parse_batch_response fails
        // and the missing block stays unrepaired.
        let repair_response = "{not valid json".to_string();

        let provider = SequenceProvider::new(vec![initial_response, repair_response]);
        let telemetry = Arc::new(TelemetryLog::new());
        let config = test_run_config();
        let translations = translate_batches_with_callback(
            provider,
            batches,
            &segments,
            &config,
            telemetry,
            None,
            None,
            Arc::new(bookforge_core::NullProgressSink),
            None,
            |_| Ok(()),
        )
        .await
        .expect("translate");

        assert_eq!(translations.len(), 1);
        let translation = &translations[0];
        assert_eq!(
            translation.status,
            SegmentStatus::NeedsReview,
            "segment with missing block translation must not be saved as Succeeded",
        );
        let error = translation
            .error
            .as_ref()
            .expect("missing-block segment must carry an error");
        assert!(
            error.contains(&missing_block_id),
            "error must name missing block id {missing_block_id}, got: {error}",
        );
    }

    #[tokio::test]
    async fn single_item_invalid_response_retries_before_needs_review() {
        let segment = make_segment("seg1", vec![plain_block("Hello")], vec![]);
        let segments = vec![segment];
        let cfg = BatchConfig {
            enabled: true,
            target_tokens: 1000,
            max_items: 1,
            adaptive_sizing: false,
            split_on_json_failure: true,
            repair_invalid_items: true,
        };
        let batches = build_translation_batches(&segments, &cfg, TranslationProfile::Balanced);
        assert_eq!(batches.len(), 1);
        let item_id = batches[0].items[0].item_id.clone();
        let provider = SequenceProvider::new(vec![
            "{not valid json".to_string(),
            serde_json::json!({
                "items": [
                    {"id": item_id, "translation": "[it] Hello"},
                ],
            })
            .to_string(),
        ]);
        let telemetry = Arc::new(TelemetryLog::new());
        let mut config = test_run_config();
        config.scheduler.max_attempts = 2;

        let translations = translate_batches_with_callback(
            provider,
            batches,
            &segments,
            &config,
            telemetry,
            None,
            None,
            Arc::new(bookforge_core::NullProgressSink),
            None,
            |_| Ok(()),
        )
        .await
        .expect("single-item invalid response should retry and succeed");

        assert_eq!(translations.len(), 1);
        assert_eq!(translations[0].status, SegmentStatus::Succeeded);
        assert_eq!(translations[0].joined_text(), "[it] Hello");
    }

    #[tokio::test]
    async fn transient_batch_errors_stop_after_max_attempts() {
        let segment = make_segment("seg1", vec![plain_block("Hello")], vec![]);
        let segments = vec![segment];
        let cfg = BatchConfig {
            enabled: true,
            target_tokens: 1000,
            max_items: 1,
            adaptive_sizing: false,
            split_on_json_failure: true,
            repair_invalid_items: true,
        };
        let batches = build_translation_batches(&segments, &cfg, TranslationProfile::Balanced);
        let provider = Arc::new(AlwaysTransientProvider::new());
        let telemetry = Arc::new(TelemetryLog::new());
        let mut config = test_run_config();
        config.scheduler.max_attempts = 2;

        let translations = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            translate_batches_with_callback(
                provider.clone(),
                batches,
                &segments,
                &config,
                telemetry,
                None,
                None,
                Arc::new(bookforge_core::NullProgressSink),
                None,
                |_| Ok(()),
            ),
        )
        .await
        .expect("transient retries must be capped")
        .expect("batch run should return needs-review translations");

        assert_eq!(provider.calls(), 2);
        assert_eq!(translations.len(), 1);
        assert_eq!(translations[0].status, SegmentStatus::NeedsReview);
        assert!(
            translations[0]
                .error
                .as_deref()
                .is_some_and(|error| error.contains("HTTP status 503")),
            "got: {:?}",
            translations[0].error
        );
    }

    #[tokio::test]
    async fn batch_finalization_preserves_source_block_order() {
        let segment = make_segment(
            "seg1",
            vec![plain_block("First"), plain_block("Second")],
            vec![],
        );
        let segments = vec![segment];
        let cfg = BatchConfig {
            enabled: true,
            target_tokens: 1000,
            max_items: 1,
            adaptive_sizing: false,
            split_on_json_failure: true,
            repair_invalid_items: true,
        };
        let batches = build_translation_batches(&segments, &cfg, TranslationProfile::Balanced);
        assert_eq!(batches.len(), 2);
        let telemetry = Arc::new(TelemetryLog::new());
        let mut config = test_run_config();
        config.scheduler.concurrency = 2;

        let translations = translate_batches_with_callback(
            DelayedPromptEchoProvider,
            batches,
            &segments,
            &config,
            telemetry,
            None,
            None,
            Arc::new(bookforge_core::NullProgressSink),
            None,
            |_| Ok(()),
        )
        .await
        .expect("translation should complete");

        assert_eq!(translations.len(), 1);
        assert_eq!(translations[0].status, SegmentStatus::Succeeded);
        assert_eq!(
            translations[0]
                .blocks
                .iter()
                .map(|block| block.text.as_str())
                .collect::<Vec<_>>(),
            vec!["[it] First", "[it] Second"]
        );
        assert_eq!(translations[0].joined_text(), "[it] First\n\n[it] Second");
    }

    #[tokio::test]
    async fn split_prerequisite_batch_unblocks_book_scoped_context_waiters() {
        let segments = vec![
            make_segment_in_section("seg0", "sec0", 0, vec![plain_block("Alpha")], vec![]),
            make_segment_in_section("seg1", "sec0", 1, vec![plain_block("Beta")], vec![]),
            make_segment_in_section("seg2", "sec1", 2, vec![plain_block("Gamma")], vec![]),
            make_segment_in_section("seg3", "sec2", 3, vec![plain_block("Delta")], vec![]),
        ];
        let cfg = BatchConfig {
            enabled: true,
            target_tokens: 1000,
            max_items: 64,
            adaptive_sizing: false,
            split_on_json_failure: true,
            repair_invalid_items: true,
        };
        let batches = build_translation_batches(&segments, &cfg, TranslationProfile::Balanced);
        assert!(batches.len() >= 3, "expected section-partitioned batches");
        assert!(batches[0].items.len() > 1, "first batch must be splittable");

        let provider = FirstInvalidThenPromptEchoProvider::new();
        let telemetry = Arc::new(TelemetryLog::new());
        let mut config = test_run_config();
        config.scheduler.concurrency = 4;
        config.context = crate::ContextRunConfig {
            window: 1,
            budget_tokens: 1000,
            scope: bookforge_core::config::ContextScope::Book,
            strict: true,
        };
        config.context_registry = Some(Arc::new(crate::ContextRegistry::new(&segments)));

        let run = translate_batches_with_callback(
            provider,
            batches,
            &segments,
            &config,
            telemetry,
            None,
            None,
            Arc::new(bookforge_core::NullProgressSink),
            None,
            |_| Ok(()),
        );

        let translations = tokio::time::timeout(std::time::Duration::from_secs(5), run)
            .await
            .expect("split prerequisite batch must not deadlock context waiters")
            .expect("translation should complete");

        assert_eq!(translations.len(), segments.len());
        assert!(
            translations
                .iter()
                .all(|translation| translation.status == SegmentStatus::Succeeded)
        );
    }

    #[tokio::test]
    async fn batch_scheduler_does_not_deadlock_when_work_and_result_queues_are_bounded() {
        // Create enough batches to stress both bounded work/result queues.
        let mut blocks = Vec::new();
        for i in 0..64 {
            blocks.push(plain_block(&format!("text_{i}")));
        }
        let segment = make_segment("seg_stress", blocks, vec![]);
        let segments = vec![segment];
        let cfg = BatchConfig {
            enabled: true,
            target_tokens: 16_000,
            max_items: 1,
            adaptive_sizing: false,
            split_on_json_failure: true,
            repair_invalid_items: true,
        };
        let batches = build_translation_batches(&segments, &cfg, TranslationProfile::Balanced);
        // With max_items = 1, we get many small batches
        assert!(batches.len() > 32, "need many batches to stress queues");

        // Use MockProvider which handles concurrent requests safely.
        use crate::provider::{MockMode, MockProvider};
        let provider = MockProvider::new(MockMode::PrefixTarget, "Italian");
        let telemetry = Arc::new(TelemetryLog::new());
        let config = test_run_config();
        let progress = Arc::new(bookforge_core::NullProgressSink);

        let run = async {
            translate_batches_with_callback(
                provider,
                batches,
                &segments,
                &config,
                telemetry,
                None,
                None,
                progress,
                None,
                |_| Ok(()),
            )
            .await
            .unwrap();
        };

        tokio::time::timeout(std::time::Duration::from_secs(10), run)
            .await
            .expect("batch scheduler must not deadlock");
    }
}
