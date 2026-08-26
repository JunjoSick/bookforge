use bookforge_core::{
    config::DoubleCheckConfig,
    ir::BlockId,
    marker::{marker_ids_in_text, marker_structure_error, strip_marker_tokens},
    segment::{Segment, SegmentId, SegmentStatus},
};
use serde::Deserialize;
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::Arc;
use tokio::{sync::Semaphore, task::JoinSet};

use crate::{
    CompletionRequest, LlmError, LlmProvider, PromptLibrary, RequestMetadata, ResponseFormat,
    SegmentTranslation, Substitutions, TranslationRunConfig, concurrency::PauseState,
};

#[derive(Debug, Clone, serde::Serialize)]
pub struct DoubleCheckItem {
    pub id: String,
    pub segment_id: String,
    pub block_id: String,
    pub section_title: Option<String>,
    pub kind: String,
    pub source: String,
    pub translation: String,
    pub required_markers: Vec<String>,
    pub protected_spans: Vec<String>,
    pub deterministic_warnings: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct DoubleCheckResponse {
    items: Vec<DoubleCheckResultItem>,
}

#[derive(Debug, Deserialize)]
struct DoubleCheckResultItem {
    id: String,
    verdict: String,
    issues: Vec<DoubleCheckIssue>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DoubleCheckIssue {
    #[allow(dead_code)]
    severity: String,
    #[allow(dead_code)]
    kind: String,
    message: String,
    #[allow(dead_code)]
    source_excerpt: Option<String>,
    #[allow(dead_code)]
    translation_excerpt: Option<String>,
    needs_correction: bool,
}

#[derive(Debug, Clone)]
pub struct CorrectionItem {
    pub item_id: String,
    pub segment_id: SegmentId,
    pub block_id: BlockId,
    pub source: String,
    pub current_translation: String,
    pub required_markers: Vec<String>,
    pub protected_spans: Vec<String>,
    pub issues: Vec<DoubleCheckIssue>,
}

#[derive(Debug, Deserialize)]
struct CorrectionResponse {
    items: Vec<CorrectionResultItem>,
}

#[derive(Debug, Deserialize)]
struct CorrectionResultItem {
    id: String,
    corrected_translation: String,
}

#[derive(Debug, Clone)]
pub enum CorrectionStatus {
    Applied,
    RejectedValidationFailed(String),
    Unresolved,
}

pub struct CorrectionRecord {
    pub item_id: String,
    pub segment_id: SegmentId,
    pub block_id: BlockId,
    pub original_translation: String,
    pub corrected_translation: Option<String>,
    pub status: CorrectionStatus,
    pub issues: Vec<DoubleCheckIssue>,
}

pub async fn run_double_check<P>(
    provider: P,
    segments: &[Segment],
    translations: &[SegmentTranslation],
    config: &TranslationRunConfig,
    double_check_config: &DoubleCheckConfig,
) -> Result<Vec<CorrectionRecord>, LlmError>
where
    P: LlmProvider,
{
    if double_check_config.mode == bookforge_core::config::DoubleCheckMode::Off {
        return Ok(Vec::new());
    }

    let library = PromptLibrary::global();
    let by_segment = segments
        .iter()
        .map(|s| (s.id.0.as_str(), s))
        .collect::<std::collections::HashMap<_, _>>();

    let mut items = Vec::new();
    let mut deterministic_issues = Vec::new();
    let validate_source_copy = crate::validation::should_validate_source_copy(
        &config.provider,
        config.source_language.as_deref(),
        &config.target_language,
    );
    for translation in translations {
        let Some(segment) = by_segment.get(translation.segment_id.0.as_str()) else {
            continue;
        };
        if !matches!(
            translation.status,
            SegmentStatus::Succeeded | SegmentStatus::SkippedCached
        ) {
            continue;
        }

        for block_t in &translation.blocks {
            let block = segment
                .source
                .blocks
                .iter()
                .find(|block| block.block_id == block_t.block_id);
            let item = DoubleCheckItem {
                id: format!("{}:{}", segment.id.0, block_t.block_id.0),
                segment_id: segment.id.0.clone(),
                block_id: block_t.block_id.0.clone(),
                section_title: segment.metadata.section_title.clone(),
                kind: block.map(|b| b.kind.clone()).unwrap_or_default(),
                source: block.map(|b| b.text.clone()).unwrap_or_default(),
                translation: block_t.text.clone(),
                required_markers: block
                    .map(|block| marker_ids_in_text(&block.text))
                    .unwrap_or_default(),
                protected_spans: block
                    .map(|block| {
                        block
                            .protected_spans
                            .iter()
                            .map(|span| span.text.clone())
                            .collect()
                    })
                    .unwrap_or_default(),
                deterministic_warnings: Vec::new(),
            };

            let copied_message = if validate_source_copy {
                crate::validation::source_copy_validation_error(
                    &item.source,
                    &item.translation,
                    item.section_title.as_deref(),
                )
            } else {
                None
            };
            if let Some(message) = copied_message {
                deterministic_issues.push(CorrectionItem {
                    item_id: item.id.clone(),
                    segment_id: SegmentId(item.segment_id.clone()),
                    block_id: BlockId(item.block_id.clone()),
                    source: item.source.clone(),
                    current_translation: item.translation.clone(),
                    required_markers: item.required_markers.clone(),
                    protected_spans: item.protected_spans.clone(),
                    issues: vec![DoubleCheckIssue {
                        severity: "high".to_string(),
                        kind: "untranslated".to_string(),
                        message,
                        source_excerpt: None,
                        translation_excerpt: None,
                        needs_correction: true,
                    }],
                });
            } else {
                items.push(item);
            }
        }
    }

    let chunk_size = double_check_config.batch_target_tokens.max(1);
    let chunks = chunk_double_check_items(&items, chunk_size);

    // Audit LLM-9: `DoubleCheckConfig.concurrency` is now honored — chunks
    // run as bounded concurrent tasks instead of one strictly sequential
    // pipeline, which previously made the configured concurrency a no-op.
    let shared_provider: Arc<P> = Arc::new(provider);
    let shared_config: Arc<TranslationRunConfig> = Arc::new(config.clone());
    let shared_double_check: Arc<DoubleCheckConfig> = Arc::new(double_check_config.clone());
    let in_flight = Arc::new(Semaphore::new(double_check_config.concurrency.max(1)));
    let mut audit_tasks = JoinSet::new();
    for chunk in chunks {
        let provider = shared_provider.clone();
        let config = shared_config.clone();
        let double_check_config = shared_double_check.clone();
        let in_flight = in_flight.clone();
        audit_tasks.spawn(async move {
            let _permit = in_flight.acquire_owned().await.ok();
            run_audit_chunk_resilient(&*provider, library, &chunk, &config, &double_check_config)
                .await
        });
    }
    let mut all_issues = deterministic_issues;
    while let Some(joined) = audit_tasks.join_next().await {
        // Stop-aware join boundary: abort before anything is persisted once
        // the run has been told to stop.
        ensure_not_stopped(config, "audit")?;
        let chunk_issues = joined.map_err(|err| {
            LlmError::Provider(format!("double-check audit task failed: {err}"))
        })??;
        all_issues.extend(chunk_issues);
    }

    if !double_check_config.auto_correct {
        let records: Vec<CorrectionRecord> = all_issues
            .into_iter()
            .map(|item| CorrectionRecord {
                item_id: item.item_id,
                segment_id: item.segment_id,
                block_id: item.block_id,
                original_translation: item.current_translation,
                corrected_translation: None,
                status: CorrectionStatus::Unresolved,
                issues: item.issues,
            })
            .collect();
        return Ok(records);
    }

    let mut records: Vec<CorrectionRecord> = all_issues
        .iter()
        .filter(|item| !item.issues.iter().any(|issue| issue.needs_correction))
        .map(|item| CorrectionRecord {
            item_id: item.item_id.clone(),
            segment_id: item.segment_id.clone(),
            block_id: item.block_id.clone(),
            original_translation: item.current_translation.clone(),
            corrected_translation: None,
            status: CorrectionStatus::Unresolved,
            issues: item.issues.clone(),
        })
        .collect();

    let correction_items: Vec<CorrectionItem> = all_issues
        .into_iter()
        .filter(|item| item.issues.iter().any(|i| i.needs_correction))
        .collect();

    // Correction responses are much more marker-sensitive than audit
    // responses. Keep substantial prose blocks isolated even when the audit
    // itself uses a large token budget.
    let correction_chunk_size = chunk_size.min(800);
    // Audit LLM-9: `correction_rounds` is honored for real instead of being
    // silently ignored. Round 1 corrects everything the audit flagged; items
    // whose correction failed validation or was omitted entirely are
    // re-sampled against their latest text on later rounds and become
    // unresolved records only once every round is spent.
    let rounds = double_check_config.correction_rounds.max(1);
    let mut pending = correction_items;
    for round in 1..=rounds {
        if pending.is_empty() {
            break;
        }
        // Stop-aware boundary before each correction round.
        ensure_not_stopped(config, "correction")?;
        let final_round = round == rounds;
        let mut correction_tasks = JoinSet::new();
        for corr_chunk in chunk_correction_items(&pending, correction_chunk_size) {
            let provider = shared_provider.clone();
            let config = shared_config.clone();
            let in_flight = in_flight.clone();
            correction_tasks.spawn(async move {
                let _permit = in_flight.acquire_owned().await.ok();
                let results: Vec<CorrectionItem> =
                    run_correction_chunk_resilient(&*provider, library, &corr_chunk, &config)
                        .await?;
                Ok::<_, LlmError>((corr_chunk, results))
            });
        }
        let mut carry_over: Vec<CorrectionItem> = Vec::new();
        while let Some(joined) = correction_tasks.join_next().await {
            let (corr_chunk, corr_results) = joined.map_err(|err| {
                LlmError::Provider(format!("double-check correction task failed: {err}"))
            })??;
            resolve_correction_chunk(
                corr_chunk,
                corr_results,
                final_round,
                &mut records,
                &mut carry_over,
            );
        }
        pending = std::mem::take(&mut carry_over);
    }

    // Final boundary: never report success for a pass whose run was stopped
    // somewhere between its begin and here.
    ensure_not_stopped(config, "pass end")?;

    Ok(records)
}

/// Fold one corrected chunk into the record list. Items that resolved leave
/// the pipeline; failures either land as terminal records (`final_round`) or
/// ride along for another sampling round with their newest text attached.
fn resolve_correction_chunk(
    corr_chunk: Vec<CorrectionItem>,
    corr_results: Vec<CorrectionItem>,
    final_round: bool,
    records: &mut Vec<CorrectionRecord>,
    carry_over: &mut Vec<CorrectionItem>,
) {
    let original_by_id: HashMap<&str, &CorrectionItem> = corr_chunk
        .iter()
        .map(|item| (item.item_id.as_str(), item))
        .collect();
    let mut returned_ids = BTreeSet::new();

    for result in corr_results {
        returned_ids.insert(result.item_id.clone());
        let Some(original) = original_by_id.get(result.item_id.as_str()) else {
            continue;
        };
        let valid = validate_correction(&result);
        let resolved = matches!(valid, CorrectionStatus::Applied) || final_round;
        if resolved {
            records.push(CorrectionRecord {
                item_id: result.item_id.clone(),
                segment_id: result.segment_id.clone(),
                block_id: result.block_id.clone(),
                original_translation: original.current_translation.clone(),
                corrected_translation: Some(result.current_translation.clone()),
                status: valid,
                issues: result.issues.clone(),
            });
        } else {
            // Keep trying: retain the model's latest attempt so the next
            // round starts from it rather than from the pre-correction text.
            carry_over.push(result);
        }
    }

    for original in &corr_chunk {
        if !returned_ids.contains(&original.item_id) {
            if final_round {
                records.push(CorrectionRecord {
                    item_id: original.item_id.clone(),
                    segment_id: original.segment_id.clone(),
                    block_id: original.block_id.clone(),
                    original_translation: original.current_translation.clone(),
                    corrected_translation: None,
                    status: CorrectionStatus::Unresolved,
                    issues: original.issues.clone(),
                });
            } else {
                carry_over.push(original.clone());
            }
        }
    }
}

fn chunk_double_check_items(
    items: &[DoubleCheckItem],
    budget_tokens: usize,
) -> Vec<Vec<DoubleCheckItem>> {
    chunk_by_budget(items, budget_tokens, estimate_double_check_item_tokens)
}

fn chunk_correction_items(
    items: &[CorrectionItem],
    budget_tokens: usize,
) -> Vec<Vec<CorrectionItem>> {
    chunk_by_budget(items, budget_tokens, estimate_correction_item_tokens)
}

fn chunk_by_budget<T: Clone>(
    items: &[T],
    budget_tokens: usize,
    estimate: impl Fn(&T) -> usize,
) -> Vec<Vec<T>> {
    let budget_tokens = budget_tokens.max(1);
    let mut chunks: Vec<Vec<T>> = Vec::new();
    let mut current: Vec<T> = Vec::new();
    let mut current_tokens = 0usize;

    for item in items {
        let item_tokens = estimate(item).max(1);
        if !current.is_empty() && current_tokens.saturating_add(item_tokens) > budget_tokens {
            chunks.push(std::mem::take(&mut current));
            current_tokens = 0;
        }
        current.push(item.clone());
        current_tokens = current_tokens.saturating_add(item_tokens);
    }

    if !current.is_empty() {
        chunks.push(current);
    }

    chunks
}

fn estimate_double_check_item_tokens(item: &DoubleCheckItem) -> usize {
    96 + estimate_text_tokens(&item.id)
        + estimate_text_tokens(&item.section_title.clone().unwrap_or_default())
        + estimate_text_tokens(&item.kind)
        + estimate_text_tokens(&item.source)
        + estimate_text_tokens(&item.translation)
        + item
            .required_markers
            .iter()
            .map(|value| estimate_text_tokens(value))
            .sum::<usize>()
        + item
            .protected_spans
            .iter()
            .map(|value| estimate_text_tokens(value))
            .sum::<usize>()
        + item
            .deterministic_warnings
            .iter()
            .map(|value| estimate_text_tokens(value))
            .sum::<usize>()
}

fn estimate_correction_item_tokens(item: &CorrectionItem) -> usize {
    96 + estimate_text_tokens(&item.item_id)
        + estimate_text_tokens(&item.source)
        + estimate_text_tokens(&item.current_translation)
        + item
            .required_markers
            .iter()
            .map(|value| estimate_text_tokens(value))
            .sum::<usize>()
        + item
            .protected_spans
            .iter()
            .map(|value| estimate_text_tokens(value))
            .sum::<usize>()
        + item
            .issues
            .iter()
            .map(|issue| {
                estimate_text_tokens(&issue.severity)
                    + estimate_text_tokens(&issue.kind)
                    + estimate_text_tokens(&issue.message)
            })
            .sum::<usize>()
}

/// Per-field text counting for double-check prompts: the canonical
/// script-aware estimator with the historical one-token floor so tiny
/// fields (ids, markers, spans) still reserve space in the chunk budget.
fn estimate_text_tokens(text: &str) -> usize {
    bookforge_core::segment::estimate_tokens(text).max(1)
}

fn is_json_shape_error(error: &LlmError) -> bool {
    matches!(error, LlmError::Json(_) | LlmError::InvalidResponse(_))
}

const AUDIT_UNRESOLVED_KIND: &str = "audit_unavailable";
const AUDIT_OMITTED_KIND: &str = "audit_omitted";

/// Cooperative stop check for double-check stage boundaries
/// (audit LLM-3/LLM-9 follow-up): a run whose control file recorded Stop at
/// any point up to pass end must not return successful corrections and let
/// the caller record success afterwards. The CLI's existing
/// `Err(_) + pause_signal.is_stopped()` branch converts this into its
/// graceful stopped path.
fn ensure_not_stopped(config: &TranslationRunConfig, stage: &str) -> Result<(), LlmError> {
    if let Some(signal) = config.pause_signal.as_ref()
        && signal.is_stopped()
    {
        return Err(LlmError::Provider(format!("double-check {stage} stopped")));
    }
    Ok(())
}

fn audit_unresolved_issue(error: &LlmError) -> DoubleCheckIssue {
    DoubleCheckIssue {
        severity: "minor".to_string(),
        kind: AUDIT_UNRESOLVED_KIND.to_string(),
        message: format!("double-check audit could not parse provider response: {error}"),
        source_excerpt: None,
        translation_excerpt: None,
        needs_correction: false,
    }
}

fn audit_omitted_issue() -> DoubleCheckIssue {
    DoubleCheckIssue {
        severity: "minor".to_string(),
        kind: AUDIT_OMITTED_KIND.to_string(),
        message: "double-check audit provider response omitted this item".to_string(),
        source_excerpt: None,
        translation_excerpt: None,
        needs_correction: false,
    }
}

async fn run_audit_chunk_resilient<P>(
    provider: &P,
    library: &PromptLibrary,
    items: &[DoubleCheckItem],
    config: &TranslationRunConfig,
    double_check_config: &DoubleCheckConfig,
) -> Result<Vec<CorrectionItem>, LlmError>
where
    P: LlmProvider,
{
    let mut queue = VecDeque::from([items.to_vec()]);
    let mut corrections = Vec::new();

    while let Some(chunk) = queue.pop_front() {
        match run_audit_chunk(provider, library, &chunk, config, double_check_config).await {
            Ok(mut result) => corrections.append(&mut result),
            Err(error) if is_json_shape_error(&error) && chunk.len() > 1 => {
                let mid = chunk.len() / 2;
                queue.push_front(chunk[mid..].to_vec());
                queue.push_front(chunk[..mid].to_vec());
            }
            Err(error) if is_json_shape_error(&error) => {
                let item = &chunk[0];
                corrections.push(CorrectionItem {
                    item_id: item.id.clone(),
                    segment_id: SegmentId(item.segment_id.clone()),
                    block_id: BlockId(item.block_id.clone()),
                    source: item.source.clone(),
                    current_translation: item.translation.clone(),
                    required_markers: item.required_markers.clone(),
                    protected_spans: item.protected_spans.clone(),
                    issues: vec![audit_unresolved_issue(&error)],
                });
            }
            Err(error) => return Err(error),
        }
    }

    Ok(corrections)
}

async fn run_audit_chunk<P>(
    provider: &P,
    library: &PromptLibrary,
    items: &[DoubleCheckItem],
    config: &TranslationRunConfig,
    double_check_config: &DoubleCheckConfig,
) -> Result<Vec<CorrectionItem>, LlmError>
where
    P: LlmProvider,
{
    let mut vars = Substitutions::new();
    vars.string(
        "source_language",
        config
            .source_language
            .as_deref()
            .unwrap_or("the source language"),
    )
    .string("target_language", &config.target_language)
    .string(
        "double_check_mode",
        double_check_mode_str(double_check_config.mode),
    )
    .json_compact("items_json", &items);

    let rendered = library
        .double_check_batch
        .render(&vars)
        .map_err(|e| LlmError::Provider(e.to_string()))?;

    wait_for_double_check_pause(config, "double-check audit").await?;
    let (runtime_config_revision, provider_max_attempts) = config.request_runtime_metadata();

    let response = provider
        .complete(CompletionRequest {
            system: rendered.system,
            user: rendered.user,
            response_format: ResponseFormat::Json,
            temperature: 0.0,
            max_output_tokens: None,
            metadata: RequestMetadata {
                prompt_template: Some(library.double_check_batch.name.clone()),
                prompt_version: Some(library.double_check_batch.version.clone()),
                provider: Some(config.provider.clone()),
                model: Some(config.model.clone()),
                runtime_config_revision,
                provider_max_attempts,
                ..RequestMetadata::default()
            },
        })
        .await?;

    let parsed: DoubleCheckResponse = serde_json::from_str(&response.content)?;

    let mut corrections = Vec::new();
    let item_map: std::collections::HashMap<&str, &DoubleCheckItem> =
        items.iter().map(|item| (item.id.as_str(), item)).collect();

    let mut seen_ids = BTreeSet::new();
    for result in &parsed.items {
        let Some(source_item) = item_map.get(result.id.as_str()) else {
            continue;
        };
        if !seen_ids.insert(result.id.clone()) {
            continue;
        }
        // Verdicts are compared case-insensitively after validation; an
        // unrecognized verdict already fails the `pass` short-circuit below
        // and is therefore preserved with its issues (conservative).
        let verdict_passes = result.verdict.trim().eq_ignore_ascii_case("pass");
        if verdict_passes && result.issues.is_empty() {
            continue;
        }
        corrections.push(CorrectionItem {
            item_id: result.id.clone(),
            segment_id: bookforge_core::segment::SegmentId(source_item.segment_id.clone()),
            block_id: BlockId(source_item.block_id.clone()),
            source: source_item.source.clone(),
            current_translation: source_item.translation.clone(),
            required_markers: source_item.required_markers.clone(),
            protected_spans: source_item.protected_spans.clone(),
            issues: result.issues.clone(),
        });
    }

    for source_item in items {
        if seen_ids.contains(source_item.id.as_str()) {
            continue;
        }
        corrections.push(CorrectionItem {
            item_id: source_item.id.clone(),
            segment_id: bookforge_core::segment::SegmentId(source_item.segment_id.clone()),
            block_id: BlockId(source_item.block_id.clone()),
            source: source_item.source.clone(),
            current_translation: source_item.translation.clone(),
            required_markers: source_item.required_markers.clone(),
            protected_spans: source_item.protected_spans.clone(),
            issues: vec![audit_omitted_issue()],
        });
    }

    Ok(corrections)
}

async fn run_correction_chunk<P>(
    provider: &P,
    library: &PromptLibrary,
    items: &[CorrectionItem],
    config: &TranslationRunConfig,
) -> Result<Vec<CorrectionItem>, LlmError>
where
    P: LlmProvider,
{
    #[derive(serde::Serialize)]
    struct CorrectionItemInput {
        id: String,
        source: String,
        current_translation: String,
        required_markers: Vec<String>,
        protected_spans: Vec<String>,
    }

    #[derive(serde::Serialize)]
    struct CorrectionIssueInput {
        severity: String,
        kind: String,
        message: String,
    }

    let item_inputs: Vec<CorrectionItemInput> = items
        .iter()
        .map(|item| CorrectionItemInput {
            id: item.item_id.clone(),
            source: item.source.clone(),
            current_translation: item.current_translation.clone(),
            required_markers: item.required_markers.clone(),
            protected_spans: item.protected_spans.clone(),
        })
        .collect();

    let issue_inputs: Vec<Vec<CorrectionIssueInput>> = items
        .iter()
        .map(|item| {
            item.issues
                .iter()
                .map(|issue| CorrectionIssueInput {
                    severity: issue.severity.clone(),
                    kind: issue.kind.clone(),
                    message: issue.message.clone(),
                })
                .collect()
        })
        .collect();

    let mut vars = Substitutions::new();
    vars.string(
        "source_language",
        config
            .source_language
            .as_deref()
            .unwrap_or("the source language"),
    )
    .string("target_language", &config.target_language)
    .json_compact("items_json", &item_inputs)
    .json_compact("issues_json", &issue_inputs);

    let rendered = library
        .correct_batch
        .render(&vars)
        .map_err(|e| LlmError::Provider(e.to_string()))?;

    wait_for_double_check_pause(config, "double-check correction").await?;
    let (runtime_config_revision, provider_max_attempts) = config.request_runtime_metadata();

    let response = provider
        .complete(CompletionRequest {
            system: rendered.system,
            user: rendered.user,
            response_format: ResponseFormat::Json,
            temperature: 0.1,
            max_output_tokens: None,
            metadata: RequestMetadata {
                prompt_template: Some(library.correct_batch.name.clone()),
                prompt_version: Some(library.correct_batch.version.clone()),
                provider: Some(config.provider.clone()),
                model: Some(config.model.clone()),
                runtime_config_revision,
                provider_max_attempts,
                ..RequestMetadata::default()
            },
        })
        .await?;

    let parsed: CorrectionResponse = serde_json::from_str(&response.content)?;

    let mut result_items = Vec::new();
    let item_map: std::collections::HashMap<&str, &CorrectionItem> = items
        .iter()
        .map(|item| (item.item_id.as_str(), item))
        .collect();

    for corr in &parsed.items {
        let Some(original) = item_map.get(corr.id.as_str()) else {
            continue;
        };
        result_items.push(CorrectionItem {
            item_id: corr.id.clone(),
            segment_id: original.segment_id.clone(),
            block_id: original.block_id.clone(),
            source: original.source.clone(),
            current_translation: corr.corrected_translation.clone(),
            required_markers: original.required_markers.clone(),
            protected_spans: original.protected_spans.clone(),
            issues: original.issues.clone(),
        });
    }

    Ok(result_items)
}

async fn run_correction_chunk_resilient<P>(
    provider: &P,
    library: &PromptLibrary,
    items: &[CorrectionItem],
    config: &TranslationRunConfig,
) -> Result<Vec<CorrectionItem>, LlmError>
where
    P: LlmProvider,
{
    let mut queue = VecDeque::from([items.to_vec()]);
    let mut corrected = Vec::new();

    while let Some(chunk) = queue.pop_front() {
        match run_correction_chunk(provider, library, &chunk, config).await {
            Ok(mut result) => corrected.append(&mut result),
            Err(error) if is_json_shape_error(&error) && chunk.len() > 1 => {
                let mid = chunk.len() / 2;
                queue.push_front(chunk[mid..].to_vec());
                queue.push_front(chunk[..mid].to_vec());
            }
            Err(error) if is_json_shape_error(&error) => {}
            Err(error) => return Err(error),
        }
    }

    Ok(corrected)
}

async fn wait_for_double_check_pause(
    config: &TranslationRunConfig,
    stage: &str,
) -> Result<(), LlmError> {
    if let Some(signal) = config.pause_signal.as_ref()
        && signal.wait_until_running_or_stopped().await == PauseState::Stopped
    {
        return Err(LlmError::Provider(format!("{stage} stopped")));
    }
    Ok(())
}

fn validate_correction(item: &CorrectionItem) -> CorrectionStatus {
    let text = &item.current_translation;

    if text.is_empty() && !item.source.is_empty() {
        return CorrectionStatus::RejectedValidationFailed(
            "corrected translation is empty".to_string(),
        );
    }

    if let Some(error) = marker_structure_error(text) {
        return CorrectionStatus::RejectedValidationFailed(error);
    }

    let mut expected_markers = item.required_markers.clone();
    expected_markers.sort();
    let mut actual_markers = marker_ids_in_text(text);
    actual_markers.sort();
    if actual_markers != expected_markers {
        return CorrectionStatus::RejectedValidationFailed(format!(
            "inline marker mismatch: expected {expected_markers:?}, got {actual_markers:?}"
        ));
    }

    for span in &item.protected_spans {
        if !text.contains(span) {
            return CorrectionStatus::RejectedValidationFailed(format!(
                "missing protected span: {span}"
            ));
        }
    }

    if item.issues.iter().any(|issue| issue.needs_correction)
        && normalized_prose(&item.source) == normalized_prose(text)
    {
        return CorrectionStatus::RejectedValidationFailed(
            "corrected translation is unchanged from the source".to_string(),
        );
    }

    CorrectionStatus::Applied
}

fn normalized_prose(text: &str) -> String {
    strip_marker_tokens(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn double_check_mode_str(mode: bookforge_core::config::DoubleCheckMode) -> &'static str {
    match mode {
        bookforge_core::config::DoubleCheckMode::Off => "off",
        bookforge_core::config::DoubleCheckMode::Formatting => "formatting",
        bookforge_core::config::DoubleCheckMode::Semantic => "semantic",
        bookforge_core::config::DoubleCheckMode::Full => "full",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicIsize, Ordering},
    };

    use super::{CorrectionStatus, DoubleCheckItem, chunk_double_check_items, run_double_check};
    use bookforge_core::{
        config::{DoubleCheckConfig, DoubleCheckMode, TranslationProfile},
        ir::{BlockId, SectionId},
        scheduler::SchedulerConfig,
        segment::{
            BlockTranslation, Segment, SegmentBlock, SegmentConstraints, SegmentContext, SegmentId,
            SegmentMetadata, SegmentSource, SegmentStatus, SegmentTextRun,
        },
    };
    use serde_json::json;

    use crate::{
        CompletionRequest, CompletionResponse, FinishReason, GlossaryRunConfig, LlmProvider,
        ProviderCapabilities, SegmentTranslation, TranslationRunConfig,
    };

    #[derive(Clone)]
    struct SequenceProvider {
        responses: Arc<Mutex<Vec<String>>>,
    }

    impl SequenceProvider {
        fn new(responses: Vec<String>) -> Self {
            let mut responses = responses;
            responses.reverse();
            Self {
                responses: Arc::new(Mutex::new(responses)),
            }
        }
    }

    impl LlmProvider for SequenceProvider {
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> crate::provider::Result<CompletionResponse> {
            let content = self
                .responses
                .lock()
                .expect("responses mutex should not be poisoned")
                .pop()
                .expect("provider response should be queued");
            Ok(CompletionResponse {
                content,
                input_tokens: Some(1),
                input_cached_tokens: Some(0),
                output_tokens: Some(1),
                finish_reason: FinishReason::Stop,
                provider_latency_ms: 0,
                raw: json!({}),
            })
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                supports_json_response_format: true,
                supports_usage_tokens: true,
            }
        }
    }

    fn segment() -> Segment {
        Segment {
            id: SegmentId("seg".to_string()),
            section_id: SectionId("sec".to_string()),
            ordinal: 0,
            block_ids: vec![BlockId("b".to_string())],
            source: SegmentSource {
                text: "source".to_string(),
                blocks: vec![SegmentBlock {
                    block_id: BlockId("b".to_string()),
                    kind: "paragraph".to_string(),
                    text: "source".to_string(),
                    text_runs: vec![SegmentTextRun {
                        id: "r".to_string(),
                        text: "source".to_string(),
                    }],
                    protected_spans: Vec::new(),
                }],
                token_estimate: 1,
            },
            context: SegmentContext::default(),
            metadata: SegmentMetadata::default(),
            constraints: SegmentConstraints::default(),
            checksum: "checksum".to_string(),
        }
    }

    fn translation() -> SegmentTranslation {
        SegmentTranslation {
            segment_id: SegmentId("seg".to_string()),
            ordinal: 0,
            block_ids: vec![BlockId("b".to_string())],
            blocks: vec![BlockTranslation {
                block_id: BlockId("b".to_string()),
                text: "errato".to_string(),
            }],
            checksum: "checksum".to_string(),
            status: SegmentStatus::Succeeded,
            template: "translate_segment".to_string(),
            error: None,
            input_tokens: Some(5),
            input_cached_tokens: Some(0),
            output_tokens: Some(6),
            tokens_estimated: false,
        }
    }

    fn segment_with_id(segment_id: &str, block_id: &str) -> Segment {
        let mut segment = segment();
        segment.id = SegmentId(segment_id.to_string());
        segment.block_ids = vec![BlockId(block_id.to_string())];
        segment.source.blocks[0].block_id = BlockId(block_id.to_string());
        segment.checksum = format!("checksum-{segment_id}");
        segment
    }

    fn translation_with_id(segment_id: &str, block_id: &str, text: &str) -> SegmentTranslation {
        let mut translation = translation();
        translation.segment_id = SegmentId(segment_id.to_string());
        translation.block_ids = vec![BlockId(block_id.to_string())];
        translation.blocks[0].block_id = BlockId(block_id.to_string());
        translation.blocks[0].text = text.to_string();
        translation.checksum = format!("checksum-{segment_id}");
        translation
    }

    fn run_config() -> TranslationRunConfig {
        TranslationRunConfig {
            source_language: Some("English".to_string()),
            target_language: "Italian".to_string(),
            provider: "test".to_string(),
            model: "test".to_string(),
            prompt_version: "v1".to_string(),
            temperature: 0.0,
            scheduler: SchedulerConfig {
                concurrency: 1,
                max_attempts: 1,
            },
            profile: TranslationProfile::Balanced,
            model_context_tokens: None,
            max_output_tokens: None,
            batch_max_output_tokens: None,
            compact_prompts: false,
            glossary: GlossaryRunConfig::default(),
            context: crate::ContextRunConfig::default(),
            context_registry: None,
            style: None,
            entities: None,
            pause_signal: None,
            runtime_settings: None,
        }
    }

    #[tokio::test]
    async fn auto_correct_records_original_and_corrected_text() {
        let provider = SequenceProvider::new(vec![
            r#"{"items":[{"id":"seg:b","verdict":"fail","issues":[{"severity":"major","kind":"formatting","message":"fix it","source_excerpt":null,"translation_excerpt":null,"needs_correction":true}]}]}"#.to_string(),
            r#"{"items":[{"id":"seg:b","corrected_translation":"corretto"}]}"#.to_string(),
        ]);
        let double_check = DoubleCheckConfig {
            mode: DoubleCheckMode::Formatting,
            model: None,
            provider: None,
            base_url: None,
            api_key_env: None,
            concurrency: 1,
            batch_target_tokens: 8_000,
            auto_correct: true,
            correction_rounds: 1,
        };

        let records = run_double_check(
            provider,
            &[segment()],
            &[translation()],
            &run_config(),
            &double_check,
        )
        .await
        .expect("double-check should succeed");

        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.segment_id.0, "seg");
        assert_eq!(record.block_id.0, "b");
        assert_eq!(record.original_translation, "errato");
        assert_eq!(record.corrected_translation.as_deref(), Some("corretto"));
        assert!(matches!(record.status, CorrectionStatus::Applied));
    }

    #[tokio::test]
    async fn cached_translations_are_included_in_double_check() {
        let provider = SequenceProvider::new(vec![
            r#"{"items":[{"id":"seg:b","verdict":"pass","issues":[]}]}"#.to_string(),
        ]);
        let double_check = DoubleCheckConfig {
            mode: DoubleCheckMode::Formatting,
            model: None,
            provider: None,
            base_url: None,
            api_key_env: None,
            concurrency: 1,
            batch_target_tokens: 8_000,
            auto_correct: false,
            correction_rounds: 1,
        };
        let mut cached = translation();
        cached.status = SegmentStatus::SkippedCached;

        let records = run_double_check(
            provider,
            &[segment()],
            &[cached],
            &run_config(),
            &double_check,
        )
        .await
        .expect("cached translation should be audited");

        assert!(records.is_empty());
    }

    #[tokio::test]
    async fn exact_long_source_copy_is_corrected_without_audit_detection() {
        let source = "This is a deliberately long English paragraph that remains identical in the \
            translation output, so the deterministic untranslated-prose guard must send it directly \
            to correction even when the selected audit mode is formatting only.";
        let provider = SequenceProvider::new(vec![
            r#"{"items":[{"id":"seg:b","corrected_translation":"Questo lungo paragrafo inglese è stato tradotto correttamente in italiano."}]}"#
                .to_string(),
        ]);
        let double_check = DoubleCheckConfig {
            mode: DoubleCheckMode::Formatting,
            model: None,
            provider: None,
            base_url: None,
            api_key_env: None,
            concurrency: 1,
            batch_target_tokens: 8_000,
            auto_correct: true,
            correction_rounds: 1,
        };
        let mut source_segment = segment();
        source_segment.source.text = source.to_string();
        source_segment.source.blocks[0].text = source.to_string();
        let copied = translation_with_id("seg", "b", source);

        let records = run_double_check(
            provider,
            &[source_segment],
            &[copied],
            &run_config(),
            &double_check,
        )
        .await
        .expect("exact source copy should be corrected");

        assert_eq!(records.len(), 1);
        assert!(matches!(records[0].status, CorrectionStatus::Applied));
    }

    #[tokio::test]
    async fn same_language_double_check_allows_unchanged_prose() {
        let source = "This is a deliberately long paragraph used for a same-language editing run, \
            where unchanged prose is valid and must still be sent through the configured audit \
            instead of being classified as an untranslated cross-language response.";
        let provider = SequenceProvider::new(vec![
            r#"{"items":[{"id":"seg:b","verdict":"pass","issues":[]}]}"#.to_string(),
        ]);
        let double_check = DoubleCheckConfig {
            mode: DoubleCheckMode::Formatting,
            model: None,
            provider: None,
            base_url: None,
            api_key_env: None,
            concurrency: 1,
            batch_target_tokens: 8_000,
            auto_correct: false,
            correction_rounds: 1,
        };
        let mut source_segment = segment();
        source_segment.source.text = source.to_string();
        source_segment.source.blocks[0].text = source.to_string();
        let copied = translation_with_id("seg", "b", source);
        let mut same_language = run_config();
        same_language.target_language = "English".to_string();

        let records = run_double_check(
            provider,
            &[source_segment],
            &[copied],
            &same_language,
            &double_check,
        )
        .await
        .expect("same-language prose should be audited normally");

        assert!(records.is_empty());
    }

    #[tokio::test]
    async fn correction_validates_only_the_markers_from_its_source_block() {
        let prose = "This long English source paragraph contains one inline marker and enough prose \
            to trigger deterministic correction because it was copied unchanged by the translator.";
        let source = format!("<m1>{prose}</m1>");
        let provider = SequenceProvider::new(vec![
            r#"{"items":[{"id":"seg:b","corrected_translation":"<m1>Questo paragrafo è ora tradotto in italiano.</m1>"}]}"#
                .to_string(),
        ]);
        let double_check = DoubleCheckConfig {
            mode: DoubleCheckMode::Formatting,
            model: None,
            provider: None,
            base_url: None,
            api_key_env: None,
            concurrency: 1,
            batch_target_tokens: 8_000,
            auto_correct: true,
            correction_rounds: 1,
        };
        let mut source_segment = segment();
        source_segment.source.text = source.clone();
        source_segment.source.blocks[0].text = source.clone();
        source_segment.constraints.preserve_markers = vec!["m1".into(), "m2".into()];
        let copied = translation_with_id("seg", "b", &source);

        let records = run_double_check(
            provider,
            &[source_segment],
            &[copied],
            &run_config(),
            &double_check,
        )
        .await
        .expect("block-local marker correction should succeed");

        assert_eq!(records.len(), 1);
        assert!(matches!(records[0].status, CorrectionStatus::Applied));
    }

    #[tokio::test]
    async fn malformed_corrected_marker_structure_is_rejected() {
        let prose = "This long English source paragraph contains one inline marker and enough prose \
            to trigger deterministic correction because it was copied unchanged by the translator.";
        let source = format!("<m1>{prose}</m1>");
        let provider = SequenceProvider::new(vec![
            r#"{"items":[{"id":"seg:b","corrected_translation":"<m1>Traduzione senza chiusura"}]}"#
                .to_string(),
        ]);
        let double_check = DoubleCheckConfig {
            mode: DoubleCheckMode::Formatting,
            model: None,
            provider: None,
            base_url: None,
            api_key_env: None,
            concurrency: 1,
            batch_target_tokens: 8_000,
            auto_correct: true,
            correction_rounds: 1,
        };
        let mut source_segment = segment();
        source_segment.source.text = source.clone();
        source_segment.source.blocks[0].text = source.clone();
        source_segment.constraints.preserve_markers = vec!["m1".into()];
        let copied = translation_with_id("seg", "b", &source);

        let records = run_double_check(
            provider,
            &[source_segment],
            &[copied],
            &run_config(),
            &double_check,
        )
        .await
        .expect("malformed correction should be recorded");

        assert_eq!(records.len(), 1);
        assert!(matches!(
            records[0].status,
            CorrectionStatus::RejectedValidationFailed(_)
        ));
    }

    #[tokio::test]
    async fn auto_correct_marks_missing_correction_unresolved() {
        let provider = SequenceProvider::new(vec![
            r#"{"items":[{"id":"seg:b","verdict":"fail","issues":[{"severity":"major","kind":"formatting","message":"fix it","source_excerpt":null,"translation_excerpt":null,"needs_correction":true}]}]}"#.to_string(),
            r#"{"items":[]}"#.to_string(),
        ]);
        let double_check = DoubleCheckConfig {
            mode: DoubleCheckMode::Formatting,
            model: None,
            provider: None,
            base_url: None,
            api_key_env: None,
            concurrency: 1,
            batch_target_tokens: 8_000,
            auto_correct: true,
            correction_rounds: 1,
        };

        let records = run_double_check(
            provider,
            &[segment()],
            &[translation()],
            &run_config(),
            &double_check,
        )
        .await
        .expect("double-check should succeed");

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].corrected_translation, None);
        assert!(matches!(records[0].status, CorrectionStatus::Unresolved));
    }

    #[tokio::test]
    async fn auto_correct_keeps_non_corrective_audit_warnings() {
        let provider = SequenceProvider::new(vec![
            r#"{"items":[{"id":"seg:b","verdict":"fail","issues":[{"severity":"minor","kind":"style","message":"awkward phrasing","source_excerpt":null,"translation_excerpt":null,"needs_correction":false}]}]}"#.to_string(),
        ]);
        let double_check = DoubleCheckConfig {
            mode: DoubleCheckMode::Formatting,
            model: None,
            provider: None,
            base_url: None,
            api_key_env: None,
            concurrency: 1,
            batch_target_tokens: 8_000,
            auto_correct: true,
            correction_rounds: 1,
        };

        let records = run_double_check(
            provider,
            &[segment()],
            &[translation()],
            &run_config(),
            &double_check,
        )
        .await
        .expect("non-corrective audit warning should be recorded");

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].item_id, "seg:b");
        assert_eq!(records[0].corrected_translation, None);
        assert!(matches!(records[0].status, CorrectionStatus::Unresolved));
        assert_eq!(records[0].issues[0].kind, "style");
    }

    #[tokio::test]
    async fn omitted_audit_items_are_recorded_unresolved() {
        let provider = SequenceProvider::new(vec![
            r#"{"items":[{"id":"seg_a:a","verdict":"pass","issues":[]}]}"#.to_string(),
        ]);
        let double_check = DoubleCheckConfig {
            mode: DoubleCheckMode::Formatting,
            model: None,
            provider: None,
            base_url: None,
            api_key_env: None,
            concurrency: 1,
            batch_target_tokens: 8_000,
            auto_correct: false,
            correction_rounds: 1,
        };

        let records = run_double_check(
            provider,
            &[segment_with_id("seg_a", "a"), segment_with_id("seg_b", "b")],
            &[
                translation_with_id("seg_a", "a", "corretto a"),
                translation_with_id("seg_b", "b", "corretto b"),
            ],
            &run_config(),
            &double_check,
        )
        .await
        .expect("omitted audit item should be recorded");

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].item_id, "seg_b:b");
        assert_eq!(records[0].corrected_translation, None);
        assert!(matches!(records[0].status, CorrectionStatus::Unresolved));
        assert_eq!(records[0].issues[0].kind, "audit_omitted");
    }

    #[tokio::test]
    async fn audit_json_error_splits_chunk_and_continues() {
        let provider = SequenceProvider::new(vec![
            "{".to_string(),
            r#"{"items":[{"id":"seg_a:a","verdict":"fail","issues":[{"severity":"minor","kind":"formatting","message":"spacing","source_excerpt":null,"translation_excerpt":null,"needs_correction":false}]}]}"#.to_string(),
            r#"{"items":[{"id":"seg_b:b","verdict":"pass","issues":[]}]}"#.to_string(),
        ]);
        let double_check = DoubleCheckConfig {
            mode: DoubleCheckMode::Formatting,
            model: None,
            provider: None,
            base_url: None,
            api_key_env: None,
            concurrency: 1,
            batch_target_tokens: 8_000,
            auto_correct: false,
            correction_rounds: 1,
        };

        let records = run_double_check(
            provider,
            &[segment_with_id("seg_a", "a"), segment_with_id("seg_b", "b")],
            &[
                translation_with_id("seg_a", "a", "errato a"),
                translation_with_id("seg_b", "b", "corretto b"),
            ],
            &run_config(),
            &double_check,
        )
        .await
        .expect("double-check should split malformed audit chunks");

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].item_id, "seg_a:a");
        assert_eq!(records[0].original_translation, "errato a");
        assert!(matches!(records[0].status, CorrectionStatus::Unresolved));
    }

    #[tokio::test]
    async fn single_item_audit_json_error_is_recorded_unresolved() {
        let provider = SequenceProvider::new(vec!["{".to_string()]);
        let double_check = DoubleCheckConfig {
            mode: DoubleCheckMode::Formatting,
            model: None,
            provider: None,
            base_url: None,
            api_key_env: None,
            concurrency: 1,
            batch_target_tokens: 8_000,
            auto_correct: true,
            correction_rounds: 1,
        };

        let records = run_double_check(
            provider,
            &[segment()],
            &[translation()],
            &run_config(),
            &double_check,
        )
        .await
        .expect("single malformed audit response should not fail the run");

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].item_id, "seg:b");
        assert_eq!(records[0].corrected_translation, None);
        assert!(matches!(records[0].status, CorrectionStatus::Unresolved));
        assert_eq!(records[0].issues[0].kind, "audit_unavailable");
    }

    #[test]
    fn double_check_chunks_use_token_budget() {
        let item = |id: &str, source_len: usize, translation_len: usize| DoubleCheckItem {
            id: id.to_string(),
            segment_id: "seg".to_string(),
            block_id: "b".to_string(),
            section_title: None,
            kind: "paragraph".to_string(),
            source: "s".repeat(source_len),
            translation: "t".repeat(translation_len),
            required_markers: Vec::new(),
            protected_spans: Vec::new(),
            deterministic_warnings: Vec::new(),
        };
        let items = vec![item("a", 800, 800), item("b", 800, 800), item("c", 10, 10)];

        let chunks = chunk_double_check_items(&items, 550);

        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0][0].id, "a");
        assert_eq!(chunks[1][0].id, "b");
        assert_eq!(chunks[2][0].id, "c");
    }

    /// Measures how many audit requests were in flight simultaneously.
    #[derive(Clone, Default)]
    struct ConcurrencyProbeProvider {
        active: Arc<AtomicIsize>,
        peak: Arc<AtomicIsize>,
    }

    impl ConcurrencyProbeProvider {
        async fn enter(&self) {
            let current = self.active.fetch_add(1, Ordering::AcqRel) + 1;
            self.peak.fetch_max(current, Ordering::AcqRel);
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            self.active.fetch_sub(1, Ordering::AcqRel);
        }

        fn peak(&self) -> isize {
            self.peak.load(Ordering::Acquire)
        }
    }

    impl LlmProvider for ConcurrencyProbeProvider {
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> crate::provider::Result<CompletionResponse> {
            self.enter().await;
            Ok(CompletionResponse {
                content: r#"{"items":[]}"#.to_string(),
                input_tokens: Some(1),
                input_cached_tokens: Some(0),
                output_tokens: Some(0),
                finish_reason: FinishReason::Stop,
                provider_latency_ms: 40,
                raw: json!({}),
            })
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                supports_json_response_format: true,
                supports_usage_tokens: true,
            }
        }
    }

    fn probe_config_with_concurrency(
        concurrency: usize,
    ) -> (TranslationRunConfig, DoubleCheckConfig) {
        let double_check = DoubleCheckConfig {
            mode: DoubleCheckMode::Formatting,
            model: None,
            provider: None,
            base_url: None,
            api_key_env: None,
            concurrency,
            batch_target_tokens: 1,
            auto_correct: false,
            correction_rounds: 1,
        };
        (run_config(), double_check)
    }

    fn two_probe_segments() -> Vec<Segment> {
        vec![segment_with_id("seg_a", "a"), segment_with_id("seg_b", "b")]
    }

    #[tokio::test]
    async fn double_check_concurrency_is_honored_not_serialized() {
        let segments = two_probe_segments();
        let translations = [
            translation_with_id("seg_a", "a", "corretto a"),
            translation_with_id("seg_b", "b", "corretto b"),
        ];
        // batch_target_tokens=1 forces one chunk per item: with two items and
        // a configured concurrency of 2 the requests must overlap.
        let (run, double_check) = probe_config_with_concurrency(2);
        let provider = ConcurrencyProbeProvider::default();

        // The stub returns an empty item list, so every id lands as
        // audit_omitted/unresolved — irrelevant here; what matters is that
        // both audit requests were in flight at once.
        let records = run_double_check(
            provider.clone(),
            &segments,
            &translations,
            &run,
            &double_check,
        )
        .await
        .expect("audit should complete");

        assert_eq!(records.len(), 2);
        assert!(
            provider.peak() >= 2,
            "configured concurrency must produce overlapped audit requests, peak={}",
            provider.peak()
        );
    }

    #[tokio::test]
    async fn second_correction_round_can_rescue_a_rejected_correction() {
        // A copied-source translation is deterministically flagged for
        // correction without any audit call, so the provider sequence here
        // is purely correction rounds: first attempt echoes the source back
        // (rejected as unchanged), the second round's re-sample succeeds.
        let prose = "This deliberately long English paragraph remains identical so the \
            deterministic untranslated-prose guard must route it to correction in \
            both rounds of this fixture before it finally resolves properly.";
        let mut source_segment = segment();
        source_segment.source.text = prose.to_string();
        source_segment.source.blocks[0].text = prose.to_string();
        let copied = translation_with_id("seg", "b", prose);
        let corrected_response = r#"{"items":[{"id":"seg:b","corrected_translation":"Questo paragrafo è stato finalmente tradotto."}]}"#;
        let echoed_source = format!(
            r#"{{"items":[{{"id":"seg:b","corrected_translation":{}}}]}}"#,
            serde_json::to_string(prose).unwrap()
        );

        let double_check = DoubleCheckConfig {
            mode: DoubleCheckMode::Formatting,
            model: None,
            provider: None,
            base_url: None,
            api_key_env: None,
            concurrency: 1,
            batch_target_tokens: 8_000,
            auto_correct: true,
            correction_rounds: 2,
        };

        // `SequenceProvider` hands out responses in FIFO order of this vec:
        // round 1 receives the echoed-source rejection, round 2 the real fix.
        let rounds_provider =
            SequenceProvider::new(vec![echoed_source, corrected_response.to_string()]);
        let records = run_double_check(
            rounds_provider,
            &[source_segment],
            &[copied],
            &run_config(),
            &double_check,
        )
        .await
        .expect("multi-round correction should resolve");

        assert_eq!(records.len(), 1);
        assert!(
            matches!(records[0].status, CorrectionStatus::Applied),
            "the second round should rescue the rejected first attempt"
        );
        assert_eq!(records[0].item_id, "seg:b");
    }
}
