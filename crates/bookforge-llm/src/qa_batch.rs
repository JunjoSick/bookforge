use crate::{
    CompletionRequest, LlmError, LlmProvider, PromptLibrary, PromptTemplate, QaIssue,
    QaSegmentReview, RequestMetadata, ResponseFormat, SegmentTranslation, TranslationRunConfig,
};
use bookforge_core::{
    config::QaRunConfig,
    segment::{Segment, SegmentId, SegmentStatus},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    sync::Arc,
};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

#[derive(Debug, Deserialize)]
struct QaBatchResponse {
    reviews: Vec<QaBatchReview>,
}

#[derive(Debug, Deserialize)]
struct QaBatchReview {
    id: String,
    verdict: String,
    #[serde(default)]
    issues: Vec<QaIssue>,
}

#[derive(Debug, Clone)]
struct QaWorkItem {
    segment: Segment,
    translation: SegmentTranslation,
}

#[derive(Debug, Serialize)]
struct QaPromptItem {
    id: String,
    segment_id: String,
    book_title: Option<String>,
    section_title: Option<String>,
    source: String,
    translation: String,
    glossary: serde_json::Value,
    required_markers: Vec<String>,
    protected_spans: Vec<String>,
}

pub async fn qa_segments_parallel<P>(
    provider: P,
    segments: &[Segment],
    translations: &[SegmentTranslation],
    config: &TranslationRunConfig,
    qa_config: &QaRunConfig,
) -> Vec<QaSegmentReview>
where
    P: LlmProvider,
{
    let library = Arc::new(PromptLibrary::global().clone());
    let provider = Arc::new(provider);
    let semaphore = Arc::new(Semaphore::new(qa_config.concurrency.max(1)));
    let mut tasks = JoinSet::new();

    let by_segment = segments
        .iter()
        .map(|segment| (segment.id.0.as_str(), segment.clone()))
        .collect::<HashMap<_, _>>();

    let mut items = Vec::new();
    let mut review_order = HashMap::new();

    for translation in translations.iter().cloned() {
        if !matches!(
            translation.status,
            SegmentStatus::Succeeded | SegmentStatus::SkippedCached
        ) {
            continue;
        }
        let Some(segment) = by_segment.get(translation.segment_id.0.as_str()).cloned() else {
            continue;
        };
        review_order.insert(translation.segment_id.0.clone(), translation.ordinal);
        items.push(QaWorkItem {
            segment,
            translation,
        });
    }

    for chunk in chunk_qa_items(&items, qa_config.batch_target_tokens) {
        let provider = provider.clone();
        let library = library.clone();
        let semaphore = semaphore.clone();
        let config = config.clone();
        let chunk_for_error = chunk.clone();

        tasks.spawn(async move {
            let Ok(_permit) = semaphore.acquire_owned().await else {
                return chunk_for_error
                    .iter()
                    .map(|item| qa_error_review(item, "qa_cancelled", "QA semaphore closed"))
                    .collect::<Vec<_>>();
            };
            request_qa_batch_resilient(&*provider, &library, &chunk, &config).await
        });
    }

    let mut reviews = Vec::with_capacity(translations.len());
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok(mut chunk_reviews) => reviews.append(&mut chunk_reviews),
            Err(err) => {
                reviews.push(QaSegmentReview {
                    segment_id: SegmentId("unknown".to_string()),
                    verdict: "error".to_string(),
                    issues: vec![QaIssue {
                        severity: "high".to_string(),
                        kind: "qa_task_panic".to_string(),
                        message: format!("QA task panicked: {err}"),
                        source_excerpt: None,
                        translation_excerpt: None,
                    }],
                });
            }
        }
    }

    reviews.sort_by_key(|review| {
        review_order
            .get(review.segment_id.0.as_str())
            .copied()
            .unwrap_or(usize::MAX)
    });
    reviews
}

async fn request_qa_batch_resilient<P>(
    provider: &P,
    library: &PromptLibrary,
    items: &[QaWorkItem],
    config: &TranslationRunConfig,
) -> Vec<QaSegmentReview>
where
    P: LlmProvider,
{
    let mut queue = VecDeque::from([items.to_vec()]);
    let mut reviews = Vec::new();

    while let Some(chunk) = queue.pop_front() {
        match request_qa_batch(provider, &library.qa_batch, &chunk, config).await {
            Ok(mut chunk_reviews) => reviews.append(&mut chunk_reviews),
            Err(error) if is_json_shape_error(&error) && chunk.len() > 1 => {
                let mid = chunk.len() / 2;
                queue.push_front(chunk[mid..].to_vec());
                queue.push_front(chunk[..mid].to_vec());
            }
            Err(error) => {
                let message = format!("QA pass failed: {error}");
                reviews.extend(
                    chunk
                        .iter()
                        .map(|item| qa_error_review(item, "qa_request_failed", &message)),
                );
            }
        }
    }

    reviews
}

async fn request_qa_batch<P>(
    provider: &P,
    template: &PromptTemplate,
    items: &[QaWorkItem],
    config: &TranslationRunConfig,
) -> Result<Vec<QaSegmentReview>, LlmError>
where
    P: LlmProvider,
{
    use crate::Substitutions;

    let prompt_items = items
        .iter()
        .map(|item| {
            let (glossary_json, _glossary_prose) =
                crate::scheduler::glossary_for_segment(config, &item.segment.id.0);
            QaPromptItem {
                id: item.segment.id.0.clone(),
                segment_id: item.segment.id.0.clone(),
                book_title: item.segment.metadata.book_title.clone(),
                section_title: item.segment.metadata.section_title.clone(),
                source: item.segment.source.text.clone(),
                translation: item.translation.joined_text(),
                glossary: glossary_json,
                required_markers: item.segment.constraints.preserve_markers.clone(),
                protected_spans: item.segment.constraints.preserve_spans.clone(),
            }
        })
        .collect::<Vec<_>>();

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
            .map(|style| style.rendered_block.clone())
            .unwrap_or_default(),
    )
    .raw(
        "entity_agreement_block",
        config
            .entities
            .as_ref()
            .map(|entities| entities.rendered_block.clone())
            .unwrap_or_default(),
    )
    .json_compact("items_json", &prompt_items);

    let rendered = template
        .render(&vars)
        .map_err(|e| LlmError::Provider(e.to_string()))?;
    let response = provider
        .complete(CompletionRequest {
            system: rendered.system,
            user: rendered.user,
            response_format: ResponseFormat::Json,
            temperature: 0.0,
            max_output_tokens: None,
            metadata: RequestMetadata {
                segment_id: None,
                block_ids: items
                    .iter()
                    .flat_map(|item| item.segment.block_ids.iter().map(|id| id.0.clone()))
                    .collect(),
                prompt_template: Some(template.name.clone()),
                prompt_version: Some(template.version.clone()),
                provider: Some(config.provider.clone()),
                model: Some(config.model.clone()),
                source_checksum: None,
            },
        })
        .await?;

    let parsed: QaBatchResponse = serde_json::from_str(&response.content)?;
    let by_id = items
        .iter()
        .map(|item| (item.segment.id.0.as_str(), item))
        .collect::<HashMap<_, _>>();
    let mut seen_ids = BTreeSet::new();
    let mut reviews = Vec::with_capacity(items.len());

    for parsed_review in parsed.reviews {
        let Some(item) = by_id.get(parsed_review.id.as_str()) else {
            continue;
        };
        if !seen_ids.insert(parsed_review.id.clone()) {
            continue;
        }
        reviews.push(QaSegmentReview {
            segment_id: item.translation.segment_id.clone(),
            verdict: parsed_review.verdict,
            issues: parsed_review.issues,
        });
    }

    for item in items {
        if seen_ids.contains(item.segment.id.0.as_str()) {
            continue;
        }
        reviews.push(qa_error_review(
            item,
            "qa_response_omitted",
            "QA provider response omitted this segment",
        ));
    }

    Ok(reviews)
}

fn chunk_qa_items(items: &[QaWorkItem], budget_tokens: usize) -> Vec<Vec<QaWorkItem>> {
    let budget_tokens = budget_tokens.max(1);
    let mut chunks = Vec::new();
    let mut current = Vec::new();
    let mut current_tokens = 0usize;

    for item in items {
        let item_tokens = estimate_qa_item_tokens(item).max(1);
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

fn estimate_qa_item_tokens(item: &QaWorkItem) -> usize {
    96 + item.segment.source.token_estimate.max(1)
        + estimate_text_tokens(&item.translation.joined_text())
        + estimate_text_tokens(&item.segment.metadata.book_title.clone().unwrap_or_default())
        + estimate_text_tokens(
            &item
                .segment
                .metadata
                .section_title
                .clone()
                .unwrap_or_default(),
        )
        + item
            .segment
            .constraints
            .preserve_markers
            .iter()
            .map(|marker| estimate_text_tokens(marker))
            .sum::<usize>()
        + item
            .segment
            .constraints
            .preserve_spans
            .iter()
            .map(|span| estimate_text_tokens(span))
            .sum::<usize>()
}

fn estimate_text_tokens(text: &str) -> usize {
    (text.chars().count() / 4).max(1)
}

fn is_json_shape_error(error: &LlmError) -> bool {
    matches!(error, LlmError::Json(_) | LlmError::InvalidResponse(_))
}

fn qa_error_review(item: &QaWorkItem, kind: &str, message: impl Into<String>) -> QaSegmentReview {
    QaSegmentReview {
        segment_id: item.translation.segment_id.clone(),
        verdict: "warn".to_string(),
        issues: vec![QaIssue {
            severity: "medium".to_string(),
            kind: kind.to_string(),
            message: message.into(),
            source_excerpt: None,
            translation_excerpt: None,
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CompletionResponse, FinishReason, ProviderCapabilities};
    use bookforge_core::{
        config::TranslationProfile,
        ir::{BlockId, SectionId},
        scheduler::SchedulerConfig,
        segment::{
            BlockTranslation, SegmentBlock, SegmentConstraints, SegmentContext, SegmentMetadata,
            SegmentSource, SegmentTextRun,
        },
    };
    use serde_json::json;
    use std::sync::Mutex;

    #[derive(Default)]
    struct CaptureQaProvider {
        requests: Arc<Mutex<Vec<CompletionRequest>>>,
        omit_last_review: bool,
    }

    impl LlmProvider for CaptureQaProvider {
        async fn complete(
            &self,
            request: CompletionRequest,
        ) -> crate::provider::Result<CompletionResponse> {
            let ids = extract_ids_from_qa_prompt(&request.user);
            self.requests.lock().expect("requests mutex").push(request);
            let review_count = if self.omit_last_review {
                ids.len().saturating_sub(1)
            } else {
                ids.len()
            };
            let reviews = ids
                .into_iter()
                .take(review_count)
                .map(|id| {
                    json!({
                        "id": id,
                        "verdict": "pass",
                        "issues": [],
                    })
                })
                .collect::<Vec<_>>();

            Ok(CompletionResponse {
                content: serde_json::to_string(&json!({ "reviews": reviews }))?,
                input_tokens: None,
                input_cached_tokens: None,
                output_tokens: None,
                finish_reason: FinishReason::Stop,
                provider_latency_ms: 0,
                raw: json!({}),
            })
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                supports_json_response_format: true,
                supports_usage_tokens: false,
            }
        }
    }

    fn extract_ids_from_qa_prompt(user_prompt: &str) -> Vec<String> {
        user_prompt
            .lines()
            .rev()
            .find_map(|line| {
                let trimmed = line.trim_start();
                if !trimmed.starts_with('[') {
                    return None;
                }
                let parsed: Vec<serde_json::Value> = serde_json::from_str(trimmed).ok()?;
                let ids = parsed
                    .into_iter()
                    .filter_map(|entry| {
                        entry
                            .get("id")
                            .and_then(serde_json::Value::as_str)
                            .map(ToString::to_string)
                    })
                    .collect::<Vec<_>>();
                if ids.is_empty() { None } else { Some(ids) }
            })
            .unwrap_or_default()
    }

    fn run_config() -> TranslationRunConfig {
        TranslationRunConfig {
            source_language: Some("English".to_string()),
            target_language: "Italian".to_string(),
            provider: "mock".to_string(),
            model: "mock".to_string(),
            prompt_version: "v1".to_string(),
            temperature: 0.0,
            scheduler: SchedulerConfig::default(),
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

    fn qa_config(batch_target_tokens: usize) -> QaRunConfig {
        QaRunConfig {
            concurrency: 1,
            batch_target_tokens,
            model: None,
            provider: None,
            base_url: None,
            api_key_env: None,
        }
    }

    fn segment(id: &str, ordinal: usize, text: &str) -> Segment {
        let block_id = BlockId(format!("block_{id}"));
        Segment {
            id: SegmentId(id.to_string()),
            section_id: SectionId("sec_01".to_string()),
            ordinal,
            block_ids: vec![block_id.clone()],
            source: SegmentSource {
                text: text.to_string(),
                blocks: vec![SegmentBlock {
                    block_id,
                    kind: "p".to_string(),
                    text: text.to_string(),
                    text_runs: vec![SegmentTextRun {
                        id: "r0".to_string(),
                        text: text.to_string(),
                    }],
                    protected_spans: Vec::new(),
                }],
                token_estimate: (text.chars().count() / 4).max(1),
            },
            context: SegmentContext::default(),
            metadata: SegmentMetadata::default(),
            constraints: SegmentConstraints::default(),
            checksum: format!("checksum_{id}"),
        }
    }

    fn translation(segment: &Segment, text: &str) -> SegmentTranslation {
        SegmentTranslation {
            segment_id: segment.id.clone(),
            ordinal: segment.ordinal,
            block_ids: segment.block_ids.clone(),
            blocks: vec![BlockTranslation {
                block_id: segment.block_ids[0].clone(),
                text: text.to_string(),
            }],
            checksum: segment.checksum.clone(),
            status: SegmentStatus::Succeeded,
            template: "translate_segment".to_string(),
            error: None,
            input_tokens: None,
            input_cached_tokens: None,
            output_tokens: None,
            tokens_estimated: false,
        }
    }

    #[tokio::test]
    async fn qa_parallel_uses_batch_prompt_when_budget_allows() {
        let segments = vec![segment("seg_1", 0, "Hello"), segment("seg_2", 1, "Goodbye")];
        let translations = segments
            .iter()
            .map(|segment| translation(segment, "Ciao"))
            .collect::<Vec<_>>();
        let provider = CaptureQaProvider::default();
        let requests = provider.requests.clone();

        let reviews = qa_segments_parallel(
            provider,
            &segments,
            &translations,
            &run_config(),
            &qa_config(10_000),
        )
        .await;

        assert_eq!(reviews.len(), 2);
        assert_eq!(reviews[0].segment_id.0, "seg_1");
        assert_eq!(reviews[1].segment_id.0, "seg_2");
        let requests = requests.lock().expect("requests mutex");
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].metadata.prompt_template.as_deref(),
            Some("qa_batch")
        );
        assert_eq!(requests[0].metadata.segment_id, None);
        assert!(requests[0].user.contains("\"id\":\"seg_1\""));
        assert!(requests[0].user.contains("\"id\":\"seg_2\""));
    }

    #[tokio::test]
    async fn qa_batch_target_tokens_splits_requests() {
        let segments = vec![segment("seg_1", 0, "Hello"), segment("seg_2", 1, "Goodbye")];
        let translations = segments
            .iter()
            .map(|segment| translation(segment, "Ciao"))
            .collect::<Vec<_>>();
        let provider = CaptureQaProvider::default();
        let requests = provider.requests.clone();

        let reviews = qa_segments_parallel(
            provider,
            &segments,
            &translations,
            &run_config(),
            &qa_config(1),
        )
        .await;

        assert_eq!(reviews.len(), 2);
        assert_eq!(requests.lock().expect("requests mutex").len(), 2);
    }

    #[tokio::test]
    async fn omitted_qa_batch_review_returns_warning_for_segment() {
        let segments = vec![segment("seg_1", 0, "Hello"), segment("seg_2", 1, "Goodbye")];
        let translations = segments
            .iter()
            .map(|segment| translation(segment, "Ciao"))
            .collect::<Vec<_>>();
        let provider = CaptureQaProvider {
            omit_last_review: true,
            ..Default::default()
        };

        let reviews = qa_segments_parallel(
            provider,
            &segments,
            &translations,
            &run_config(),
            &qa_config(10_000),
        )
        .await;

        assert_eq!(reviews.len(), 2);
        let omitted = reviews
            .iter()
            .find(|review| review.segment_id.0 == "seg_2")
            .expect("seg_2 should receive an omitted review warning");
        assert_eq!(omitted.verdict, "warn");
        assert_eq!(omitted.issues[0].kind, "qa_response_omitted");
    }
}
