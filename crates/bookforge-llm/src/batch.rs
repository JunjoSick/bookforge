use bookforge_core::{
    config::{BatchConfig, ProviderRequestMetric, TranslationProfile},
    ir::BlockId,
    segment::{BlockTranslation, Segment, SegmentId, SegmentStatus, SegmentTextRun},
};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::{
    AdaptiveLimiter, CompletionRequest, LlmError, LlmProvider, PromptLibrary, RequestMetadata,
    ResponseFormat, SegmentTranslation, Substitutions, TelemetryLog, TranslationRunConfig,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BatchMode {
    Plain,
    MarkerSafe,
    RunPreserving,
    TurboTextOnly,
}

#[derive(Debug, Clone)]
pub struct TranslationBatch {
    pub id: String,
    pub ordinal: usize,
    pub mode: BatchMode,
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
            if chars[i] == '<' && i + 1 < chars.len() && chars[i + 1] != '/' {
                let rest: String = chars[i..].iter().take(20).collect();
                if rest.contains('>') {
                    let end = chars[i..].iter().position(|&c| c == '>').unwrap_or(0);
                    i += end + 1;
                    out.push(' ');
                    continue;
                }
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

fn group_batches(
    items: Vec<TranslationBatchItem>,
    config: &BatchConfig,
) -> Vec<TranslationBatch> {
    let mut mode_groups: HashMap<BatchMode, Vec<TranslationBatchItem>> = HashMap::new();
    for item in items {
        mode_groups.entry(item.mode()).or_default().push(item);
    }

    let target_tokens = mode_target_tokens(config.target_tokens);
    let mut batches = Vec::new();
    let mut batch_ordinal = 0usize;

    for (mode, group_items) in mode_groups {
        let token_limit = target_tokens.get(&mode).copied().unwrap_or(config.target_tokens);
        let max_items = config.max_items;

        let mut current: Vec<TranslationBatchItem> = Vec::new();
        let mut current_tokens = 0usize;

        for item in group_items {
            let item_tokens = token_estimate(&item.source_text);
            let would_exceed_tokens = !current.is_empty()
                && current_tokens + item_tokens > token_limit;
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
        items,
        token_estimate,
    }
}

fn mode_target_tokens(base: usize) -> HashMap<BatchMode, usize> {
    let mut map = HashMap::new();
    map.insert(BatchMode::Plain, base);
    map.insert(
        BatchMode::MarkerSafe,
        base.min(10_000),
    );
    map.insert(
        BatchMode::RunPreserving,
        base.min(4_000),
    );
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

        if item.translation.is_empty()
            && !request_item.source_text.is_empty()
        {
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

pub async fn translate_batches_with_callback<P, F>(
    provider: P,
    batches: Vec<TranslationBatch>,
    segments: &[Segment],
    config: &TranslationRunConfig,
    telemetry: Arc<TelemetryLog>,
    limiter: Option<Arc<AdaptiveLimiter>>,
    mut on_segment: F,
) -> Result<Vec<SegmentTranslation>, LlmError>
where
    P: LlmProvider,
    F: FnMut(&SegmentTranslation) -> Result<(), LlmError>,
{
    let library = Arc::new(PromptLibrary::embedded());
    let provider = Arc::new(provider);
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

        let active_concurrency = if let Some(ref l) = limiter {
            l.current()
        } else {
            concurrency
        };

        let round_semaphore = Arc::new(Semaphore::new(active_concurrency));
        let mut tasks = JoinSet::new();

        for batch in pending.drain(..) {
            let provider = provider.clone();
            let library = library.clone();
            let config = config.clone();
            let semaphore = round_semaphore.clone();
            let telemetry = telemetry.clone();
            let limiter = limiter.clone();

            tasks.spawn(async move {
                let Ok(_permit) = semaphore.acquire_owned().await else {
                    return (batch, Err(LlmError::Provider(
                        "scheduler semaphore closed".to_string(),
                    )));
                };
                let started = std::time::Instant::now();
                let result = translate_one_batch(provider, library, batch.clone(), &config).await;
                let latency_ms = started.elapsed().as_millis() as u64;
                let metric = ProviderRequestMetric {
                    request_id: format!("batch_{}", batch.id),
                    batch_id: Some(batch.id.clone()),
                    provider: config.provider.clone(),
                    model: config.model.clone(),
                    profile: format!("{:?}", config.profile),
                    items: batch.items.len(),
                    estimated_input_tokens: batch.token_estimate,
                    max_output_tokens: Some(batch_max_output_tokens(&batch, config.profile)),
                    input_tokens: result.as_ref().ok().and_then(|r| r.input_tokens),
                    output_tokens: result.as_ref().ok().and_then(|r| r.output_tokens),
                    latency_ms,
                    finish_reason: None,
                    status: if result.is_ok() { "ok".into() } else { "error".into() },
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
                        Err(LlmError::Http(e)) if e.is_timeout() => l.on_timeout(),
                        _ => {}
                    }
                }

                (batch, result)
            });
        }

        while let Some(task_result) = tasks.join_next().await {
            match task_result {
                Ok((_batch, Ok(batch_result))) => {
                    all_results.push(batch_result);
                }
                Ok((batch, Err(LlmError::InvalidResponse(_)))) if batch.items.len() > 1 => {
                    eprintln!(
                        "batch {} failed with invalid response, splitting into {} + {} items",
                        batch.id,
                        batch.items.len() / 2,
                        batch.items.len() - batch.items.len() / 2,
                    );
                    pending.extend(split_batch(&batch));
                }
                Ok((batch, Err(error))) => {
                    eprintln!("batch {} failed: {error}", batch.id);
                    all_results.push(BatchTranslationResult {
                        batch_id: batch.id.clone(),
                        translations: Vec::new(),
                        failures: batch.items.iter().map(|item| BatchItemFailure {
                            item_id: item.item_id.clone(),
                            segment_id: item.segment_id.clone(),
                            error: format!("{error}"),
                        }).collect(),
                        input_tokens: None,
                        output_tokens: None,
                    });
                }
                Err(err) => {
                    eprintln!("batch task panicked: {err}");
                }
            }
        }
    }

    for batch in &pending {
        all_results.push(BatchTranslationResult {
            batch_id: batch.id.clone(),
            translations: Vec::new(),
            failures: batch.items.iter().map(|item| BatchItemFailure {
                item_id: item.item_id.clone(),
                segment_id: item.segment_id.clone(),
                error: "batch exhausted retries after split".to_string(),
            }).collect(),
            input_tokens: None,
            output_tokens: None,
        });
    }

    let mut segment_translations: HashMap<String, SegmentTranslation> = HashMap::new();

    let ordinal_by_segment: HashMap<&str, usize> = segments
        .iter()
        .map(|s| (s.id.0.as_str(), s.ordinal))
        .collect();

    for batch_result in &all_results {
        for translation in &batch_result.translations {
            let seg_id = translation.segment_id.0.clone();
            let ordinal = ordinal_by_segment
                .get(seg_id.as_str())
                .copied()
                .unwrap_or(0);
            let entry = segment_translations
                .entry(seg_id.clone())
                .or_insert_with(|| SegmentTranslation {
                    segment_id: SegmentId(seg_id.clone()),
                    ordinal,
                    block_ids: Vec::new(),
                    blocks: Vec::new(),
                    checksum: String::new(),
                    status: SegmentStatus::Succeeded,
                    template: "batch".to_string(),
                    error: None,
                    input_tokens: Some(batch_result.input_tokens.unwrap_or(0)),
                    output_tokens: Some(batch_result.output_tokens.unwrap_or(0)),
                });
            entry.blocks.push(BlockTranslation {
                block_id: BlockId(translation.item_id.clone()),
                text: translation.text.clone(),
            });
        }

        for failure in &batch_result.failures {
            let seg_id = failure.segment_id.0.clone();
            segment_translations
                .entry(seg_id.clone())
                .or_insert_with(|| SegmentTranslation {
                    segment_id: SegmentId(seg_id),
                    ordinal: 0,
                    block_ids: Vec::new(),
                    blocks: Vec::new(),
                    checksum: String::new(),
                    status: SegmentStatus::NeedsReview,
                    template: "batch".to_string(),
                    error: Some(failure.error.clone()),
                    input_tokens: None,
                    output_tokens: None,
                });
        }
    }

    let repair_items: Vec<(BatchItemFailure, TranslationBatchItem)> = all_results
        .iter()
        .flat_map(|r| &r.failures)
        .filter(|f| f.segment_id.0 != "unknown")
        .filter_map(|f| {
            all_items.get(f.item_id.as_str()).map(|item| (f.clone(), (*item).clone()))
        })
        .collect();

    if !repair_items.is_empty() {
        let repair_batch = TranslationBatch {
            id: "repair".to_string(),
            ordinal: 999,
            mode: BatchMode::Plain,
            items: repair_items.iter().map(|(_, item)| item.clone()).collect(),
            token_estimate: repair_items.iter().map(|(_, item)| token_estimate(&item.source_text)).sum(),
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
        vars.raw("items_json", serde_json::to_string(&items_json).unwrap_or_default())
            .raw("errors_json", serde_json::to_string(&errors_json).unwrap_or_default());

        #[allow(clippy::collapsible_if)]
        if let Ok(rendered) = library.batch_repair.render(&vars) {
            if let Ok(response) = provider.complete(CompletionRequest {
                system: rendered.system,
                user: rendered.user,
                response_format: ResponseFormat::Json,
                temperature: 0.1,
                max_output_tokens: Some(batch_max_output_tokens(&repair_batch, config.profile)),
                metadata: RequestMetadata::default(),
            }).await {
                if let Ok(repaired) = parse_batch_response(&repair_batch, &response.content) {
                    for translation in repaired.translations {
                        if let Some(existing) = segment_translations.get_mut(&translation.segment_id.0) {
                            existing.status = SegmentStatus::Succeeded;
                            existing.error = None;
                            if let Some(block) = existing.blocks.iter_mut().find(|b| b.block_id.0 == translation.item_id) {
                                block.text = translation.text;
                            }
                        }
                    }
                }
            }
        }
    }

    let mut translations: Vec<SegmentTranslation> = segment_translations.into_values().collect();
    for translation in &mut translations {
        on_segment(translation)?;
    }

    Ok(translations)
}

fn batch_max_output_tokens(batch: &TranslationBatch, profile: TranslationProfile) -> u32 {
    let multiplier = match batch.mode {
        BatchMode::Plain => 3,
        BatchMode::MarkerSafe => 4,
        BatchMode::RunPreserving => 5,
        BatchMode::TurboTextOnly => 2,
    };
    let estimate = batch.token_estimate as u32 * multiplier;
    let max = if profile == TranslationProfile::FreeTier { 4_096 } else { 16_384 };
    estimate.clamp(512, max)
}

async fn translate_one_batch(
    provider: Arc<impl LlmProvider>,
    library: Arc<PromptLibrary>,
    batch: TranslationBatch,
    config: &TranslationRunConfig,
) -> Result<BatchTranslationResult, LlmError> {
    let items_json = render_batch_items(&batch);
    let template = match batch.mode {
        BatchMode::Plain | BatchMode::TurboTextOnly => &library.batch_plain,
        BatchMode::MarkerSafe => &library.batch_marker_safe,
        BatchMode::RunPreserving => &library.batch_run_preserving,
    };

    let mut vars = Substitutions::new();
    vars.string("source_language", config.source_language.as_deref().unwrap_or("the source language"))
        .string("target_language", &config.target_language)
        .raw("items_json", items_json);

    let rendered = template
        .render(&vars)
        .map_err(|e| LlmError::Provider(e.to_string()))?;

    let max_tokens = batch_max_output_tokens(&batch, config.profile);

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
            let mut result = parse_batch_response(&batch, &resp.content)
                .map_err(LlmError::InvalidResponse)?;
            result.input_tokens = resp.input_tokens;
            result.output_tokens = resp.output_tokens;
            Ok(result)
        }
        Err(LlmError::InvalidResponse(msg)) if msg.contains("truncated") => {
            let result = BatchTranslationResult {
                batch_id: batch.id.clone(),
                translations: Vec::new(),
                failures: batch
                    .items
                    .iter()
                    .map(|item| BatchItemFailure {
                        item_id: item.item_id.clone(),
                        segment_id: item.segment_id.clone(),
                        error: format!("batch output truncated: {msg}"),
                    })
                    .collect(),
                input_tokens: None,
                output_tokens: None,
            };
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

#[cfg(test)]
mod tests {
    use super::*;
    use bookforge_core::segment::{SegmentBlock, SegmentConstraints, SegmentContext, SegmentId, SegmentMetadata, SegmentSource, SegmentTextRun};

    fn make_segment(id: &str, blocks: Vec<SegmentBlock>, markers: Vec<String>) -> Segment {
        Segment {
            id: SegmentId(id.to_string()),
            section_id: bookforge_core::ir::SectionId("sec_000000".to_string()),
            ordinal: 0,
            block_ids: blocks.iter().map(|b| b.block_id.clone()).collect(),
            source: SegmentSource {
                text: blocks.iter().map(|b| b.text.clone()).collect::<Vec<_>>().join("\n"),
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
        let batches = build_translation_batches(&[seg1, seg2], &config, TranslationProfile::Balanced);
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
        let batches = build_translation_batches(&[seg1, seg2], &config, TranslationProfile::Balanced);
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
        let batches = build_translation_batches(&[seg1, seg2], &config, TranslationProfile::Balanced);
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
        let batches = build_translation_batches(&[seg1, seg2, seg3, seg4], &config, TranslationProfile::Balanced);
        let split = split_batch(&batches[0]);
        assert_eq!(split.len(), 2);
        assert_eq!(split[0].items.len(), 2);
        assert_eq!(split[1].items.len(), 2);
    }}
