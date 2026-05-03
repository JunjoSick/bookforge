use bookforge_core::{
    config::QaRunConfig,
    segment::{Segment, SegmentStatus},
};
use crate::{
    PromptLibrary, PromptTemplate, QaIssue, QaSegmentReview, SegmentTranslation,
    TranslationRunConfig, CompletionRequest, LlmError, LlmProvider, RequestMetadata,
    ResponseFormat,
};
use std::sync::Arc;
use serde::Deserialize;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

#[derive(Debug, Deserialize)]
struct QaResponse {
    segment_id: String,
    verdict: String,
    issues: Vec<QaIssue>,
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
    let library = Arc::new(PromptLibrary::embedded());
    let provider = Arc::new(provider);
    let semaphore = Arc::new(Semaphore::new(qa_config.concurrency.max(1)));
    let mut tasks = JoinSet::new();

    let by_segment = segments
        .iter()
        .map(|segment| (segment.id.0.as_str(), segment.clone()))
        .collect::<std::collections::HashMap<_, _>>();

    for translation in translations.iter().cloned() {
        if translation.status != SegmentStatus::Succeeded {
            continue;
        }
        let Some(segment) = by_segment.get(translation.segment_id.0.as_str()).cloned() else {
            continue;
        };
        let provider = provider.clone();
        let library = library.clone();
        let semaphore = semaphore.clone();
        let config = config.clone();

        tasks.spawn(async move {
            let Ok(_permit) = semaphore.acquire_owned().await else {
                return QaSegmentReview {
                    segment_id: translation.segment_id.clone(),
                    verdict: "warn".to_string(),
                    issues: vec![QaIssue {
                        severity: "medium".to_string(),
                        kind: "qa_cancelled".to_string(),
                        message: "QA semaphore closed".to_string(),
                        source_excerpt: None,
                        translation_excerpt: None,
                    }],
                };
            };
            request_qa_parallel(&*provider, &library, &segment, &translation, &config).await
        });
    }

    let mut reviews = Vec::with_capacity(translations.len());
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok(review) => reviews.push(review),
            Err(err) => {
                reviews.push(QaSegmentReview {
                    segment_id: bookforge_core::segment::SegmentId("unknown".to_string()),
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

    reviews
}

async fn request_qa_parallel<P>(
    provider: &P,
    library: &PromptLibrary,
    segment: &Segment,
    translation: &SegmentTranslation,
    config: &TranslationRunConfig,
) -> QaSegmentReview
where
    P: LlmProvider,
{
    match request_qa_single(provider, &library.qa, segment, translation, config).await {
        Ok(review) => review,
        Err(error) => QaSegmentReview {
            segment_id: translation.segment_id.clone(),
            verdict: "warn".to_string(),
            issues: vec![QaIssue {
                severity: "medium".to_string(),
                kind: "qa_request_failed".to_string(),
                message: format!("QA pass failed: {error}"),
                source_excerpt: None,
                translation_excerpt: None,
            }],
        },
    }
}

async fn request_qa_single<P>(
    provider: &P,
    template: &PromptTemplate,
    segment: &Segment,
    translation: &SegmentTranslation,
    config: &TranslationRunConfig,
) -> Result<QaSegmentReview, LlmError>
where
    P: LlmProvider,
{
    use crate::Substitutions;
    let mut vars = Substitutions::new();
    vars.string("segment_id", &segment.id.0)
        .string("source_language", config.source_language.as_deref().unwrap_or("the source language"))
        .string("target_language", &config.target_language)
        .string("source_text", &segment.source.text)
        .string("translation_text", translation.joined_text())
        .json("required_markers", &segment.constraints.preserve_markers)
        .json("protected_spans", &segment.constraints.preserve_spans);

    let rendered = template.render(&vars).map_err(|e| LlmError::Provider(e.to_string()))?;
    let response = provider
        .complete(CompletionRequest {
            system: rendered.system,
            user: rendered.user,
            response_format: ResponseFormat::Json,
            temperature: 0.0,
            max_output_tokens: None,
            metadata: RequestMetadata {
                segment_id: Some(segment.id.0.clone()),
                block_ids: segment.block_ids.iter().map(|id| id.0.clone()).collect(),
                prompt_template: Some(template.name.clone()),
                prompt_version: Some(template.version.clone()),
                provider: Some(config.provider.clone()),
                model: Some(config.model.clone()),
                source_checksum: Some(segment.checksum.clone()),
            },
        })
        .await?;

    let parsed: QaResponse = serde_json::from_str(&response.content)?;
    if parsed.segment_id != segment.id.0 {
        return Err(LlmError::InvalidResponse(format!(
            "QA response segment_id mismatch: expected {}, got {}",
            segment.id.0, parsed.segment_id
        )));
    }

    Ok(QaSegmentReview {
        segment_id: segment.id.clone(),
        verdict: parsed.verdict,
        issues: parsed.issues,
    })
}
