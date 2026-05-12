use bookforge_core::{
    config::{BatchConfig, ProviderRequestMetric, TranslationProfile},
    glossary::GlossaryFormat,
    ir::BlockId,
    segment::{BlockTranslation, Segment, SegmentId, SegmentStatus, SegmentTextRun},
};
use serde::Deserialize;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::{sync::mpsc, task::JoinSet};

use crate::{
    CompletionRequest, FinishReason, LlmError, LlmProvider, PromptLibrary, ProviderRateController,
    RequestMetadata, RequestStatus, ResponseFormat, SegmentTranslation, Substitutions,
    TelemetryLog, TranslationRunConfig,
};

struct BatchWorkerOutput {
    batch: TranslationBatch,
    result: Result<BatchTranslationResult, LlmError>,
    request_status: RequestStatus,
    latency_ms: u64,
    max_output_tokens: u32,
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
}

#[derive(Debug, Clone)]
pub struct TranslationBatchItem {
    pub item_id: String,
    pub segment_id: SegmentId,
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
                    segment.constraints.preserve_markers.clone(),
                    block.protected_spans.clone(),
                )
            };

            items.push(TranslationBatchItem {
                item_id: format!("{}:{}", segment.id.0, block.block_id.0),
                segment_id: segment.id.clone(),
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
    let mut mode_groups: HashMap<BatchMode, Vec<TranslationBatchItem>> = HashMap::new();
    for item in items {
        mode_groups.entry(item.mode()).or_default().push(item);
    }

    let target_tokens = mode_target_tokens(config.target_tokens);
    let mut batches = Vec::new();
    let mut batch_ordinal = 0usize;

    for (mode, group_items) in mode_groups {
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
) -> TranslationBatch {
    TranslationBatch {
        id,
        ordinal,
        mode,
        kind: BatchKind::Translation,
        items,
        token_estimate,
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
    if entries.is_empty() {
        return estimate;
    }
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

fn restore_missing_tokens(mut translation: String, required: &[String]) -> String {
    for token in required {
        if !translation.contains(token) {
            if !translation.is_empty() && !translation.ends_with(char::is_whitespace) {
                translation.push(' ');
            }
            translation.push_str(token);
        }
    }
    translation
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
    let content = response_json.trim();

    match batch.mode {
        BatchMode::Plain | BatchMode::MarkerSafe | BatchMode::TurboTextOnly => {
            parse_text_batch_response(batch, content, batch.mode == BatchMode::TurboTextOnly)
        }
        BatchMode::RunPreserving => parse_run_batch_response(batch, content),
    }
}

fn parse_text_batch_response(
    batch: &TranslationBatch,
    content: &str,
    turbo: bool,
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

        let mut translation = item.translation.clone();
        if !turbo {
            translation = restore_missing_tokens(translation, &request_item.required_markers);
            translation = restore_missing_tokens(translation, &request_item.protected_spans);
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

        let expected_ids: HashMap<&str, ()> = request_item
            .text_runs
            .iter()
            .map(|r| (r.id.as_str(), ()))
            .collect();

        for run in &item.runs {
            if !expected_ids.contains_key(run.id.as_str()) {
                failures.push(BatchItemFailure {
                    item_id: item.id.clone(),
                    segment_id: request_item.segment_id.clone(),
                    error: format!("unknown run ID in response: {}", run.id),
                    input_tokens: None,
                    input_cached_tokens: None,
                    output_tokens: None,
                    tokens_estimated: false,
                });
                break;
            }
        }

        let joined: Vec<String> = item.runs.iter().map(|r| r.text.clone()).collect();
        translations.push(BatchItemTranslation {
            item_id: item.id.clone(),
            segment_id: request_item.segment_id.clone(),
            text: joined.join(""),
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
        ));
    }
    if !right.is_empty() {
        batches.push(make_batch(
            format!("{}_split_1", batch.id),
            batch.ordinal * 2 + 1,
            batch.mode,
            right.to_vec(),
            batch_token_estimate(right, config),
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

#[allow(clippy::too_many_arguments)]
pub async fn translate_batches_with_callback<P, F>(
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
) -> Result<Vec<SegmentTranslation>, LlmError>
where
    P: LlmProvider,
    F: FnMut(&SegmentTranslation) -> Result<(), LlmError>,
{
    let library = Arc::new(PromptLibrary::global().clone());
    let provider = Arc::new(provider);
    let config = Arc::new(config.clone());
    let concurrency = config.scheduler.concurrency.max(1);

    let all_items: HashMap<String, TranslationBatchItem> = batches
        .iter()
        .flat_map(|b| b.items.iter())
        .map(|item| (item.item_id.clone(), item.clone()))
        .collect();

    let mut all_results: Vec<BatchTranslationResult> = Vec::new();
    let mut pending: Vec<TranslationBatch> = batches;
    let max_rounds = 3usize;

    for _round in 0..max_rounds {
        if pending.is_empty() {
            break;
        }

        // Keep one spawned task per in-flight batch. This avoids the
        // persistent-worker/result-channel deadlock class where a worker logs
        // request completion but the coordinator never observes a result.
        let mut pending_queue: VecDeque<TranslationBatch> = pending.drain(..).collect();
        let mut tasks = JoinSet::<BatchWorkerOutput>::new();

        while !pending_queue.is_empty() || !tasks.is_empty() {
            while tasks.len() < concurrency {
                let Some(batch) = pending_queue.pop_front() else {
                    break;
                };
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

                tasks.spawn(async move {
                    let permit = match rate_controller.as_ref() {
                        Some(controller) => match controller.acquire().await {
                            Ok(permit) => Some(permit),
                            Err(_) => {
                                return BatchWorkerOutput {
                                    batch,
                                    result: Err(LlmError::Provider(
                                        "adaptive concurrency limiter closed".to_string(),
                                    )),
                                    request_status: RequestStatus::OtherError,
                                    latency_ms: 0,
                                    max_output_tokens: 0,
                                };
                            }
                        },
                        None => None,
                    };

                    let started = std::time::Instant::now();
                    let is_reasoning = provider.is_reasoning();

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
                        max_output_tokens: Some(capped_batch_max_output_tokens(
                            &batch,
                            &config,
                            is_reasoning,
                        )),
                        active_requests: 0,
                        target_concurrency: config.scheduler.concurrency,
                        timestamp_ms: bookforge_core::progress::now_ms(),
                    });

                    let max_output_tokens =
                        capped_batch_max_output_tokens(&batch, &config, is_reasoning);
                    let result = translate_one_batch(
                        provider.clone(),
                        library.clone(),
                        batch.clone(),
                        &config,
                    )
                    .await;
                    let latency_ms = started.elapsed().as_millis() as u64;

                    let request_status = request_status_for_controller(&result);

                    drop(permit);
                    BatchWorkerOutput {
                        batch,
                        result,
                        request_status,
                        latency_ms,
                        max_output_tokens,
                    }
                });
            }

            let Some(joined) = tasks.join_next().await else {
                continue;
            };
            let BatchWorkerOutput {
                batch,
                result,
                request_status,
                latency_ms,
                max_output_tokens,
            } = joined
                .map_err(|err| LlmError::Provider(format!("batch worker task failed: {err}")))?;

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
                    if let Some(ref mut sizer) = batch_sizer {
                        sizer.on_success_for_mode(batch.mode, latency_ms);
                    }
                    all_results.push(batch_result);
                }
                Err(LlmError::InvalidResponse(_)) if batch.kind == BatchKind::Repair => {
                    progress.emit(bookforge_core::ProgressEvent::Warning {
                        kind: "repair_batch_invalid_response".to_string(),
                        message: format!(
                            "repair batch {} failed; marking {} items NeedsReview",
                            batch.id,
                            batch.items.len()
                        ),
                        timestamp_ms: bookforge_core::progress::now_ms(),
                    });
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
                Err(LlmError::InvalidResponse(_)) if batch.items.len() == 1 => {
                    progress.emit(bookforge_core::ProgressEvent::Warning {
                        kind: "single_item_batch_invalid_response".to_string(),
                        message: format!(
                            "single-item batch {} failed; not splitting further",
                            batch.id
                        ),
                        timestamp_ms: bookforge_core::progress::now_ms(),
                    });
                    all_results.push(BatchTranslationResult {
                        batch_id: batch.id.clone(),
                        translations: Vec::new(),
                        failures: batch
                            .items
                            .iter()
                            .map(|item| BatchItemFailure {
                                item_id: item.item_id.clone(),
                                segment_id: item.segment_id.clone(),
                                error: "single-item batch invalid response".to_string(),
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
                Err(LlmError::InvalidResponse(_)) if batch.items.len() > 1 => {
                    if let Some(ref mut sizer) = batch_sizer {
                        if request_status == RequestStatus::Truncated {
                            sizer.on_truncation_for_mode(batch.mode);
                        } else {
                            sizer.on_invalid_json_for_mode(batch.mode);
                        }
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
                        kind: "batch_invalid_response_split".to_string(),
                        message: format!(
                            "batch {} failed with invalid response, splitting",
                            batch.id
                        ),
                        timestamp_ms: bookforge_core::progress::now_ms(),
                    });
                    pending_queue.extend(split);
                }
                Err(ref error) if is_transient(error) && batch.kind == BatchKind::Translation => {
                    progress.emit(bookforge_core::ProgressEvent::Warning {
                        kind: "batch_transient_retry".to_string(),
                        message: format!("batch {} transient error, retrying: {error}", batch.id),
                        timestamp_ms: bookforge_core::progress::now_ms(),
                    });
                    pending_queue.push_back(batch);
                }
                Err(error) => {
                    progress.emit(bookforge_core::ProgressEvent::Warning {
                        kind: "batch_failed".to_string(),
                        message: format!("batch {} failed: {error}", batch.id),
                        timestamp_ms: bookforge_core::progress::now_ms(),
                    });
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
            let entry = segment_translations
                .entry(seg_id.clone())
                .or_insert_with(|| {
                    make_entry(
                        &seg_id,
                        SegmentStatus::NeedsReview,
                        Some(failure.error.clone()),
                        None,
                        None,
                        None,
                        false,
                    )
                });
            add_failure_usage(entry, failure);
        }
    }

    let repair_items: Vec<(BatchItemFailure, TranslationBatchItem)> = all_results
        .iter()
        .flat_map(|r| &r.failures)
        .filter(|f| f.segment_id.0 != "unknown")
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
                                    match parse_batch_response(&repair_batch, &response.content) {
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
        let expected: std::collections::HashSet<&str> = translation
            .block_ids
            .iter()
            .map(|id| id.0.as_str())
            .collect();
        let actual: std::collections::HashSet<&str> = translation
            .blocks
            .iter()
            .map(|block| block.block_id.0.as_str())
            .collect();

        if expected != actual {
            let mut missing: Vec<&str> = expected.difference(&actual).copied().collect();
            missing.sort_unstable();
            translation.status = SegmentStatus::NeedsReview;
            translation.error = Some(format!(
                "batch translation missing block translations: {:?}",
                missing
            ));
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

fn batch_max_output_tokens(
    batch: &TranslationBatch,
    profile: TranslationProfile,
    reasoning: bool,
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
    let estimate = batch.token_estimate as u32 * multiplier;
    let max = if profile == TranslationProfile::FreeTier {
        if reasoning { 8_192 } else { 4_096 }
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
    let computed = batch_max_output_tokens(batch, config.profile, reasoning);
    let user_cap = config.batch_max_output_tokens.or(config.max_output_tokens);
    bookforge_core::config::cap_output_tokens(
        computed,
        batch.token_estimate,
        config.model_context_tokens,
        user_cap,
    )
}

async fn translate_one_batch(
    provider: Arc<impl LlmProvider>,
    library: Arc<PromptLibrary>,
    batch: TranslationBatch,
    config: &TranslationRunConfig,
) -> Result<BatchTranslationResult, LlmError> {
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
        "prompt_extra",
        config.glossary.prompt_extra.clone().unwrap_or_default(),
    )
    .raw("items_json", items_json);

    let rendered = template
        .render(&vars)
        .map_err(|e| LlmError::Provider(e.to_string()))?;

    let max_tokens = capped_batch_max_output_tokens(&batch, config, provider.is_reasoning());

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

            let mut result =
                parse_batch_response(&batch, &resp.content).map_err(LlmError::InvalidResponse)?;
            result.input_tokens = resp.input_tokens;
            result.input_cached_tokens = resp.input_cached_tokens;
            result.output_tokens = resp.output_tokens;
            apportion_batch_usage(&batch, &mut result);
            Ok(result)
        }
        Err(e) => Err(e),
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
        Segment {
            id: SegmentId(id.to_string()),
            section_id: bookforge_core::ir::SectionId("sec_000000".to_string()),
            ordinal: 0,
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
            checksum: "abc".to_string(),
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

    fn batch_item(id: &str, source_text: &str) -> TranslationBatchItem {
        TranslationBatchItem {
            item_id: id.to_string(),
            segment_id: SegmentId(format!("seg_{id}")),
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
    fn restores_missing_protected_tokens_in_batch_response() {
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

        let result = parse_batch_response(batch, &response).expect("parse");
        assert_eq!(result.failures.len(), 0);
        assert_eq!(result.translations.len(), 1);
        assert!(result.translations[0].text.contains("4th"));
        assert!(result.translations[0].text.contains("<bf:keep/>"));
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
    use std::sync::Mutex;

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

        let rendered = render_batch_items(&batch, &config);
        assert!(rendered.contains("\"glossary\""));
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

        let result = translate_one_batch(provider, library, batch, &config).await;
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

        let result = translate_one_batch(provider, library, batch, &config).await;
        match result {
            Err(LlmError::InvalidResponse(msg)) => {
                assert!(msg.contains("truncated"), "unexpected msg: {msg}")
            }
            Ok(_) => panic!("truncated error must not be swallowed into Ok"),
            other => panic!("expected InvalidResponse, got {other:?}"),
        }
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
