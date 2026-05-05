use bookforge_core::{
    config::{BatchConfig, ProviderRequestMetric, TranslationProfile},
    ir::BlockId,
    segment::{BlockTranslation, Segment, SegmentId, SegmentStatus, SegmentTextRun},
};
use serde::Deserialize;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::{Mutex as TokioMutex, Semaphore, mpsc};

use crate::{
    AdaptiveLimiter, CompletionRequest, FinishReason, LlmError, LlmProvider, PromptLibrary,
    RequestMetadata, ResponseFormat, SegmentTranslation, Substitutions, TelemetryLog,
    TranslationRunConfig, concurrency::AdaptivePermit,
};

#[allow(dead_code)]
enum BatchPermit {
    Adaptive(AdaptivePermit),
    Fixed(tokio::sync::OwnedSemaphorePermit),
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
    pub output_tokens: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct BatchItemTranslation {
    pub item_id: String,
    pub segment_id: SegmentId,
    pub text: String,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct BatchItemFailure {
    pub item_id: String,
    pub segment_id: SegmentId,
    pub error: String,
}

#[derive(Clone)]
pub struct BatchSizer {
    target_tokens: usize,
    max_items: usize,
    #[allow(dead_code)]
    original_target_tokens: usize,
    original_max_items: usize,
    progress: Option<Arc<dyn bookforge_core::ProgressSink>>,
}

impl BatchSizer {
    pub fn new(target_tokens: usize, max_items: usize) -> Self {
        Self {
            target_tokens,
            max_items,
            original_target_tokens: target_tokens,
            original_max_items: max_items,
            progress: None,
        }
    }

    pub fn with_progress(
        target_tokens: usize,
        max_items: usize,
        progress: Arc<dyn bookforge_core::ProgressSink>,
    ) -> Self {
        Self {
            target_tokens,
            max_items,
            original_target_tokens: target_tokens,
            original_max_items: max_items,
            progress: Some(progress),
        }
    }

    pub fn target_tokens(&self) -> usize {
        self.target_tokens
    }

    pub fn max_items(&self) -> usize {
        self.max_items
    }

    fn emit_change(&self, reason: &str, prev_target: usize, prev_max: usize) {
        if let Some(ref p) = self.progress {
            p.emit(bookforge_core::ProgressEvent::BatchSizingChanged {
                batch_id: None,
                previous_target: prev_target,
                new_target: self.target_tokens,
                previous_max_items: prev_max,
                new_max_items: self.max_items,
                reason: reason.to_string(),
                timestamp_ms: bookforge_core::progress::now_ms(),
            });
        }
    }

    pub fn on_truncation(&mut self) {
        let prev_target = self.target_tokens;
        let prev_max = self.max_items;
        self.target_tokens = ((self.target_tokens as f64) * 0.65) as usize;
        self.max_items = ((self.max_items as f64) * 0.75) as usize;
        self.clamp();
        self.emit_change("truncation", prev_target, prev_max);
    }

    pub fn on_invalid_json(&mut self) {
        let prev_target = self.target_tokens;
        let prev_max = self.max_items;
        self.target_tokens = ((self.target_tokens as f64) * 0.75) as usize;
        self.max_items = ((self.max_items as f64) * 0.85) as usize;
        self.clamp();
        self.emit_change("invalid_json", prev_target, prev_max);
    }

    pub fn on_p95_high(&mut self) {
        self.target_tokens = ((self.target_tokens as f64) * 0.85) as usize;
        self.clamp();
    }

    pub fn on_success(&mut self) {
        self.target_tokens = ((self.target_tokens as f64) * 1.10) as usize;
        self.clamp();
    }

    fn clamp(&mut self) {
        // Per-mode clamp ranges: Plain/Turbo 4k..32k, MarkerSafe 2k..16k, RunPreserving 1k..8k
        // Use a conservative general clamp; per-mode limits would need mode tracking.
        self.target_tokens = self.target_tokens.clamp(1_000, 32_000);
        self.max_items = self.max_items.clamp(1, self.original_max_items.max(128));
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
            });
            continue;
        }

        if !turbo && !request_item.required_markers.is_empty() {
            let mut missing = Vec::new();
            for marker in &request_item.required_markers {
                if !item.translation.contains(marker) {
                    missing.push(marker.clone());
                }
            }
            if !missing.is_empty() {
                failures.push(BatchItemFailure {
                    item_id: item.id.clone(),
                    segment_id: request_item.segment_id.clone(),
                    error: format!("missing required markers: {:?}", missing),
                });
                continue;
            }
        }

        if !turbo {
            for span in &request_item.protected_spans {
                if !item.translation.contains(span) {
                    failures.push(BatchItemFailure {
                        item_id: item.id.clone(),
                        segment_id: request_item.segment_id.clone(),
                        error: format!("missing protected span: {span}"),
                    });
                    break;
                }
            }
        }

        translations.push(BatchItemTranslation {
            item_id: item.id.clone(),
            segment_id: request_item.segment_id.clone(),
            text: item.translation.clone(),
            input_tokens: None,
            output_tokens: None,
        });
    }

    for item in &batch.items {
        if !seen.contains_key(item.item_id.as_str()) {
            failures.push(BatchItemFailure {
                item_id: item.item_id.clone(),
                segment_id: item.segment_id.clone(),
                error: "item missing from batch response".to_string(),
            });
        }
    }

    Ok(BatchTranslationResult {
        batch_id: batch.id.clone(),
        translations,
        failures,
        input_tokens: None,
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
            output_tokens: None,
        });
    }

    for item in &batch.items {
        if !seen.contains_key(item.item_id.as_str()) {
            failures.push(BatchItemFailure {
                item_id: item.item_id.clone(),
                segment_id: item.segment_id.clone(),
                error: "item missing from batch response".to_string(),
            });
        }
    }

    Ok(BatchTranslationResult {
        batch_id: batch.id.clone(),
        translations,
        failures,
        input_tokens: None,
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
            left.iter().map(|i| token_estimate(&i.source_text)).sum(),
        ));
    }
    if !right.is_empty() {
        batches.push(make_batch(
            format!("{}_split_1", batch.id),
            batch.ordinal * 2 + 1,
            batch.mode,
            right.to_vec(),
            right.iter().map(|i| token_estimate(&i.source_text)).sum(),
        ));
    }
    batches
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
    limiter: Option<Arc<AdaptiveLimiter>>,
    mut batch_sizer: Option<&mut BatchSizer>,
    progress: Arc<dyn bookforge_core::ProgressSink>,
    finalized_tx: Option<mpsc::Sender<SegmentTranslation>>,
    mut on_segment: F,
) -> Result<Vec<SegmentTranslation>, LlmError>
where
    P: LlmProvider,
    F: FnMut(&SegmentTranslation) -> Result<(), LlmError>,
{
    let library = Arc::new(PromptLibrary::embedded());
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

    // Bounded work/result channels. Workers pull work after acquiring a
    // permit, so the queue size just needs to handle brief bursts.
    // Result channel uses a large capacity to prevent deadlock from
    // interleaved work dispatch and result collection.
    let queue_size = concurrency * 4;
    let (work_tx, work_rx) = mpsc::channel::<TranslationBatch>(queue_size);
    let (result_tx, mut result_rx) = mpsc::channel::<(
        TranslationBatch,
        Result<BatchTranslationResult, LlmError>,
    )>(queue_size * 16);
    let work_rx = Arc::new(TokioMutex::new(work_rx));

    // Fixed-concurrency semaphore only used when no adaptive limiter is configured.
    let fixed_semaphore = if limiter.is_none() {
        Some(Arc::new(Semaphore::new(concurrency)))
    } else {
        None
    };

    // Spawn persistent worker tasks.
    let mut worker_handles = Vec::new();
    for _ in 0..concurrency {
        let provider = provider.clone();
        let library = library.clone();
        let config = config.clone();
        let telemetry = telemetry.clone();
        let limiter = limiter.clone();
        let fixed_semaphore = fixed_semaphore.clone();
        let work_rx = work_rx.clone();
        let result_tx = result_tx.clone();
        let progress = progress.clone();

        let handle = tokio::spawn(async move {
            loop {
                // Acquire permit BEFORE pulling work (per addendum rule).
                let permit = match (&limiter, &fixed_semaphore) {
                    (Some(l), _) => match l.acquire().await {
                        Ok(p) => BatchPermit::Adaptive(p),
                        Err(_) => break,
                    },
                    (None, Some(sem)) => match sem.clone().acquire_owned().await {
                        Ok(p) => BatchPermit::Fixed(p),
                        Err(_) => break,
                    },
                    (None, None) => unreachable!(),
                };

                // Pull work from the shared receiver.
                let batch = {
                    let mut rx = work_rx.lock().await;
                    rx.recv().await
                };
                let Some(batch) = batch else {
                    drop(permit);
                    break;
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

                let result =
                    translate_one_batch(provider.clone(), library.clone(), batch.clone(), &config)
                        .await;
                let latency_ms = started.elapsed().as_millis() as u64;

                progress.emit(bookforge_core::ProgressEvent::RequestFinished {
                    request_id,
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

                let metric = ProviderRequestMetric {
                    request_id: format!("batch_{}", batch.id),
                    batch_id: Some(batch.id.clone()),
                    provider: config.provider.clone(),
                    model: config.model.clone(),
                    profile: format!("{:?}", config.profile),
                    items: batch.items.len(),
                    estimated_input_tokens: batch.token_estimate,
                    max_output_tokens: Some(capped_batch_max_output_tokens(
                        &batch,
                        &config,
                        is_reasoning,
                    )),
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
                };
                telemetry.record(metric);

                if let Some(ref l) = limiter {
                    match &result {
                        Ok(_) => l.on_success(),
                        Err(LlmError::HttpStatus { status: 429, .. }) => l.on_rate_limit(),
                        Err(LlmError::HttpStatus { status, .. }) if *status >= 500 => {
                            l.on_timeout()
                        }
                        Err(LlmError::Http(e)) if e.is_timeout() || e.is_connect() => {
                            l.on_timeout()
                        }
                        _ => {}
                    }
                }

                drop(permit);
                let _ = result_tx.send((batch, result)).await;
            }
        });
        worker_handles.push(handle);
    }

    for _round in 0..max_rounds {
        if pending.is_empty() {
            break;
        }

        // Push batches to workers, draining results as needed to avoid
        // deadlock from both bounded queues filling.
        let mut pending_queue: VecDeque<TranslationBatch> = pending.drain(..).collect();
        let mut in_flight = 0usize;

        while !pending_queue.is_empty() || in_flight > 0 {
            // Try to push work to channel
            while let Some(batch) = pending_queue.front() {
                match work_tx.try_send(batch.clone()) {
                    Ok(()) => {
                        pending_queue.pop_front();
                        in_flight += 1;
                    }
                    Err(mpsc::error::TrySendError::Full(_)) => break,
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        pending_queue.clear();
                        break;
                    }
                }
            }

            // Drain a single result to free space in both channels
            if in_flight > 0 {
                let Some((batch, result)) = result_rx.recv().await else {
                    break;
                };
                in_flight -= 1;

                match result {
                    Ok(batch_result) => {
                        if let Some(ref mut sizer) = batch_sizer {
                            sizer.on_success();
                        }
                        all_results.push(batch_result);
                    }
                    Err(LlmError::InvalidResponse(_)) if batch.kind == BatchKind::Repair => {
                        eprintln!(
                            "repair batch {} failed; marking {} items NeedsReview",
                            batch.id,
                            batch.items.len(),
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
                                })
                                .collect(),
                            input_tokens: None,
                            output_tokens: None,
                        });
                    }
                    Err(LlmError::InvalidResponse(_)) if batch.items.len() == 1 => {
                        eprintln!(
                            "single-item batch {} failed; not splitting further",
                            batch.id,
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
                                    error: "single-item batch invalid response".to_string(),
                                })
                                .collect(),
                            input_tokens: None,
                            output_tokens: None,
                        });
                    }
                    Err(LlmError::InvalidResponse(_)) if batch.items.len() > 1 => {
                        if let Some(ref mut sizer) = batch_sizer {
                            sizer.on_invalid_json();
                        }
                        eprintln!("batch {} failed with invalid response, splitting", batch.id);
                        pending_queue.extend(split_batch(&batch));
                    }
                    Err(ref error)
                        if is_transient(error) && batch.kind == BatchKind::Translation =>
                    {
                        eprintln!("batch {} transient error, retrying: {error}", batch.id);
                        pending_queue.push_back(batch);
                    }
                    Err(error) => {
                        eprintln!("batch {} failed: {error}", batch.id);
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
                                })
                                .collect(),
                            input_tokens: None,
                            output_tokens: None,
                        });
                    }
                }
            }
        }
        pending = pending_queue.into();
    }

    // Signal workers to exit and wait for them.
    drop(work_tx);
    for handle in worker_handles {
        let _ = handle.await;
    }

    let mut segment_translations: HashMap<String, SegmentTranslation> = HashMap::new();

    let segments_by_id: HashMap<&str, &Segment> =
        segments.iter().map(|s| (s.id.0.as_str(), s)).collect();

    let make_entry = |seg_id: &str,
                      status: SegmentStatus,
                      error: Option<String>,
                      input_tokens: Option<u64>,
                      output_tokens: Option<u64>|
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
                output_tokens,
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
                output_tokens,
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
                        Some(batch_result.input_tokens.unwrap_or(0)),
                        Some(batch_result.output_tokens.unwrap_or(0)),
                    )
                });
            if let Some(source_item) = all_items.get(&translation.item_id) {
                entry.blocks.push(BlockTranslation {
                    block_id: source_item.block_id.clone(),
                    text: translation.text.clone(),
                });
            } else {
                eprintln!(
                    "batch translation item_id {} missing from all_items; skipping (internal state bug)",
                    translation.item_id,
                );
            }
        }

        for failure in &batch_result.failures {
            let seg_id = failure.segment_id.0.clone();
            segment_translations
                .entry(seg_id.clone())
                .or_insert_with(|| {
                    make_entry(
                        &seg_id,
                        SegmentStatus::NeedsReview,
                        Some(failure.error.clone()),
                        None,
                        None,
                    )
                });
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

    if !repair_items.is_empty() {
        let repair_batch = TranslationBatch {
            id: "repair".to_string(),
            ordinal: 999,
            mode: BatchMode::Plain,
            kind: BatchKind::Repair,
            items: repair_items.iter().map(|(_, item)| item.clone()).collect(),
            token_estimate: repair_items
                .iter()
                .map(|(_, item)| token_estimate(&item.source_text))
                .sum(),
        };

        let items_json: Vec<serde_json::Value> = repair_items
            .iter()
            .map(|(_failure, item)| {
                serde_json::json!({
                    "id": item.item_id,
                    "source_text": item.source_text,
                    "required_markers": item.required_markers,
                    "protected": item.protected_spans,
                })
            })
            .collect();

        let errors_json: Vec<serde_json::Value> = repair_items
            .iter()
            .map(|(failure, _)| serde_json::json!({"id": failure.item_id, "error": failure.error}))
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

        #[allow(clippy::collapsible_if)]
        let repair_template = if config.compact_prompts {
            &library.batch_repair_compact
        } else {
            &library.batch_repair
        };
        #[allow(clippy::collapsible_if)]
        if let Ok(rendered) = repair_template.render(&vars) {
            #[allow(clippy::collapsible_if)]
            if let Ok(response) = provider
                .complete(CompletionRequest {
                    system: rendered.system,
                    user: rendered.user,
                    response_format: ResponseFormat::Json,
                    temperature: 0.1,
                    max_output_tokens: Some(capped_batch_max_output_tokens(
                        &repair_batch,
                        &config,
                        provider.is_reasoning(),
                    )),
                    metadata: RequestMetadata::default(),
                })
                .await
            {
                if let Ok(repaired) = parse_batch_response(&repair_batch, &response.content) {
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
                        }
                    }
                }
            }
        }
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

    for translation in &mut translations {
        if let Some(ref tx) = finalized_tx {
            let _ = tx.send(translation.clone()).await;
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
    let items_json = render_batch_items(&batch);
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
            result.output_tokens = resp.output_tokens;
            Ok(result)
        }
        Err(e) => Err(e),
    }
}

fn render_batch_items(batch: &TranslationBatch) -> String {
    let items: Vec<serde_json::Value> = batch
        .items
        .iter()
        .map(|item| {
            let base = serde_json::json!({
                "id": item.item_id,
                "kind": item.kind,
                "text": item.source_text,
                "required_markers": item.required_markers,
                "protected": item.protected_spans,
            });

            if batch.mode == BatchMode::RunPreserving {
                let mut obj = base.as_object().cloned().unwrap_or_default();
                let runs: Vec<serde_json::Value> = item
                    .text_runs
                    .iter()
                    .map(|r| serde_json::json!({"id": r.id, "text": r.text}))
                    .collect();
                obj.insert("runs".to_string(), serde_json::Value::Array(runs));
                serde_json::Value::Object(obj)
            } else {
                base
            }
        })
        .collect();

    serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_string())
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

    #[test]
    fn plain_blocks_batch_together() {
        let seg1 = make_segment("seg1", vec![plain_block("Hello world")], vec![]);
        let seg2 = make_segment("seg2", vec![plain_block("Goodbye world")], vec![]);
        let config = BatchConfig {
            enabled: true,
            target_tokens: 1000,
            max_items: 64,
            split_on_json_failure: true,
            repair_invalid_items: true,
        };
        let batches =
            build_translation_batches(&[seg1, seg2], &config, TranslationProfile::Balanced);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].items.len(), 2);
    }

    #[test]
    fn parses_valid_batch_response() {
        let seg1 = make_segment("seg1", vec![plain_block("Hello")], vec![]);
        let seg2 = make_segment("seg2", vec![plain_block("Goodbye")], vec![]);
        let config = BatchConfig {
            enabled: true,
            target_tokens: 1000,
            max_items: 64,
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
    fn detects_missing_items_in_batch_response() {
        let seg1 = make_segment("seg1", vec![plain_block("Hello")], vec![]);
        let seg2 = make_segment("seg2", vec![plain_block("Goodbye")], vec![]);
        let config = BatchConfig {
            enabled: true,
            target_tokens: 1000,
            max_items: 64,
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
        }
    }

    fn make_two_item_batch() -> TranslationBatch {
        let seg1 = make_segment("seg1", vec![plain_block("Hello")], vec![]);
        let seg2 = make_segment("seg2", vec![plain_block("Goodbye")], vec![]);
        let config = BatchConfig {
            enabled: true,
            target_tokens: 1000,
            max_items: 64,
            split_on_json_failure: true,
            repair_invalid_items: true,
        };
        build_translation_batches(&[seg1, seg2], &config, TranslationProfile::Balanced)
            .into_iter()
            .next()
            .unwrap()
    }

    #[tokio::test]
    async fn batch_length_finish_reason_returns_invalid_response() {
        let batch = make_two_item_batch();
        let provider = Arc::new(StubProvider::new(StubBehavior::FinishLength));
        let library = Arc::new(PromptLibrary::embedded());
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
        let library = Arc::new(PromptLibrary::embedded());
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
