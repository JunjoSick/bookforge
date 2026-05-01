use std::sync::Arc;

use bookforge_core::{
    scheduler::SchedulerConfig,
    segment::{Segment, SegmentId},
};
use serde::Deserialize;
use tokio::{sync::Semaphore, task::JoinSet};

use crate::provider::{
    CompletionRequest, LlmError, LlmProvider, RequestMetadata, ResponseFormat, Result,
};

#[derive(Debug, Clone)]
pub struct TranslationRunConfig {
    pub source_language: Option<String>,
    pub target_language: String,
    pub provider: String,
    pub model: String,
    pub prompt_version: String,
    pub temperature: f32,
    pub scheduler: SchedulerConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentTranslation {
    pub segment_id: SegmentId,
    pub ordinal: usize,
    pub checksum: String,
    pub text: String,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct PlainTranslationResponse {
    segment_id: String,
    translation: String,
}

pub async fn translate_segments<P>(
    provider: P,
    segments: &[Segment],
    config: &TranslationRunConfig,
) -> Result<Vec<SegmentTranslation>>
where
    P: LlmProvider,
{
    if config.scheduler.concurrency == 0 {
        return Err(LlmError::Provider(
            "scheduler concurrency must be greater than zero".to_string(),
        ));
    }

    let provider = Arc::new(provider);
    let semaphore = Arc::new(Semaphore::new(config.scheduler.concurrency));
    let mut tasks = JoinSet::new();

    for segment in segments.iter().cloned() {
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|err| LlmError::Provider(err.to_string()))?;
        let provider = provider.clone();
        let config = config.clone();

        tasks.spawn(async move {
            let _permit = permit;
            translate_one(provider, segment, config).await
        });
    }

    let mut translations = Vec::with_capacity(segments.len());
    while let Some(result) = tasks.join_next().await {
        let translation = result.map_err(|err| LlmError::Provider(err.to_string()))??;
        translations.push(translation);
    }

    translations.sort_by_key(|translation| translation.ordinal);
    Ok(translations)
}

async fn translate_one<P>(
    provider: Arc<P>,
    segment: Segment,
    config: TranslationRunConfig,
) -> Result<SegmentTranslation>
where
    P: LlmProvider,
{
    let attempts = config.scheduler.max_retries.max(1);
    let mut last_error = None;

    for _attempt in 0..attempts {
        match request_translation(provider.as_ref(), &segment, &config).await {
            Ok(translation) => return Ok(translation),
            Err(error) => last_error = Some(error),
        }
    }

    Err(last_error.unwrap_or_else(|| {
        LlmError::Provider(format!(
            "segment '{}' exhausted retries without an error",
            segment.id.0
        ))
    }))
}

async fn request_translation<P>(
    provider: &P,
    segment: &Segment,
    config: &TranslationRunConfig,
) -> Result<SegmentTranslation>
where
    P: LlmProvider,
{
    let request = CompletionRequest {
        system: render_plain_system_prompt(config),
        user: segment.source.text.clone(),
        response_format: ResponseFormat::Json,
        temperature: config.temperature,
        max_output_tokens: None,
        metadata: RequestMetadata {
            segment_id: Some(segment.id.0.clone()),
            prompt_version: Some(config.prompt_version.clone()),
            provider: Some(config.provider.clone()),
            model: Some(config.model.clone()),
            source_checksum: Some(segment.checksum.clone()),
        },
    };
    let response = provider.complete(request).await?;
    let parsed: PlainTranslationResponse = serde_json::from_str(&response.content)?;

    if parsed.segment_id != segment.id.0 {
        return Err(LlmError::InvalidResponse(format!(
            "segment id mismatch: expected '{}', got '{}'",
            segment.id.0, parsed.segment_id
        )));
    }

    Ok(SegmentTranslation {
        segment_id: segment.id.clone(),
        ordinal: segment.ordinal,
        checksum: segment.checksum.clone(),
        text: parsed.translation,
        input_tokens: response.input_tokens,
        output_tokens: response.output_tokens,
    })
}

fn render_plain_system_prompt(config: &TranslationRunConfig) -> String {
    format!(
        "Translate from {} to {}. Return only valid JSON matching {{\"segment_id\":\"...\",\"translation\":\"...\"}}.",
        config.source_language.as_deref().unwrap_or("auto"),
        config.target_language
    )
}

#[cfg(test)]
mod tests {
    use bookforge_core::segment::{Segment, SegmentConstraints, SegmentContext, SegmentSource};

    use super::*;
    use crate::provider::{MockMode, MockProvider};

    #[tokio::test]
    async fn mock_scheduler_returns_ordered_translations() {
        let segments = vec![segment("seg_b", 1, "Second"), segment("seg_a", 0, "First")];
        let config = config();

        let translations = translate_segments(
            MockProvider::new(MockMode::PrefixTarget, "Italian"),
            &segments,
            &config,
        )
        .await
        .expect("mock translation should succeed");

        assert_eq!(translations[0].segment_id.0, "seg_a");
        assert_eq!(translations[1].segment_id.0, "seg_b");
        assert_eq!(translations[0].text, "[Italian] First");
    }

    #[tokio::test]
    async fn scheduler_rejects_invalid_segment_id_response() {
        let segments = vec![segment("seg_a", 0, "First")];
        let error = translate_segments(
            MockProvider::new(MockMode::WrongSegmentId, "Italian"),
            &segments,
            &config(),
        )
        .await
        .expect_err("wrong segment id should fail validation");

        assert!(error.to_string().contains("segment id mismatch"));
    }

    fn config() -> TranslationRunConfig {
        TranslationRunConfig {
            source_language: Some("English".to_string()),
            target_language: "Italian".to_string(),
            provider: "mock".to_string(),
            model: "mock-prefix".to_string(),
            prompt_version: "translate_segment.v1".to_string(),
            temperature: 0.2,
            scheduler: SchedulerConfig {
                concurrency: 2,
                max_retries: 1,
            },
        }
    }

    fn segment(id: &str, ordinal: usize, text: &str) -> Segment {
        Segment {
            id: SegmentId(id.to_string()),
            section_id: bookforge_core::ir::SectionId("sec".to_string()),
            ordinal,
            block_ids: Vec::new(),
            source: SegmentSource {
                text: text.to_string(),
                token_estimate: 1,
            },
            context: SegmentContext::default(),
            constraints: SegmentConstraints {
                preserve_markers: Vec::new(),
                preserve_spans: Vec::new(),
                max_tokens: 100,
            },
            checksum: id.to_string(),
        }
    }
}
