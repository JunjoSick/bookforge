use std::sync::Arc;

use bookforge_core::{
    scheduler::SchedulerConfig,
    segment::{Segment, SegmentId, SegmentStatus},
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
    pub status: SegmentStatus,
    pub error: Option<String>,
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
            Err(error) if is_validator_error(&error) => last_error = Some(error),
            Err(error) => return Err(error),
        }
    }

    let error = last_error.unwrap_or_else(|| {
        LlmError::InvalidResponse(format!(
            "segment '{}' exhausted validation retries without an error",
            segment.id.0
        ))
    });

    Ok(SegmentTranslation {
        segment_id: segment.id.clone(),
        ordinal: segment.ordinal,
        checksum: segment.checksum.clone(),
        text: segment.source.text.clone(),
        status: SegmentStatus::NeedsReview,
        error: Some(error.to_string()),
        input_tokens: None,
        output_tokens: None,
    })
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

    validate_translation(segment, &parsed.translation)?;

    Ok(SegmentTranslation {
        segment_id: segment.id.clone(),
        ordinal: segment.ordinal,
        checksum: segment.checksum.clone(),
        text: parsed.translation,
        status: SegmentStatus::Succeeded,
        error: None,
        input_tokens: response.input_tokens,
        output_tokens: response.output_tokens,
    })
}

fn is_validator_error(error: &LlmError) -> bool {
    matches!(error, LlmError::InvalidResponse(_) | LlmError::Json(_))
}

fn validate_translation(segment: &Segment, translation: &str) -> Result<()> {
    for span in &segment.constraints.preserve_spans {
        if !translation.contains(span) {
            return Err(LlmError::InvalidResponse(format!(
                "protected span missing from segment '{}': {}",
                segment.id.0, span
            )));
        }
    }

    for marker in &segment.constraints.preserve_markers {
        let occurrences = translation.matches(marker).count();
        if occurrences != 1 {
            return Err(LlmError::InvalidResponse(format!(
                "marker '{}' appears {} times in segment '{}'",
                marker, occurrences, segment.id.0
            )));
        }
    }

    Ok(())
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
        assert_eq!(translations[0].status, SegmentStatus::Succeeded);
    }

    #[tokio::test]
    async fn validator_failure_becomes_needs_review() {
        let segments = vec![segment("seg_a", 0, "First")];
        let translations = translate_segments(
            MockProvider::new(MockMode::WrongSegmentId, "Italian"),
            &segments,
            &config(),
        )
        .await
        .expect("validator failure should preserve source after retries");

        assert_eq!(translations[0].status, SegmentStatus::NeedsReview);
        assert_eq!(translations[0].text, "First");
        assert!(
            translations[0]
                .error
                .as_deref()
                .is_some_and(|error| error.contains("segment id mismatch"))
        );
    }

    #[tokio::test]
    async fn protected_span_failure_becomes_needs_review() {
        let mut segment = segment("seg_a", 0, "Visit https://example.com");
        segment
            .constraints
            .preserve_spans
            .push("https://example.com".to_string());
        let translations = translate_segments(
            MockProvider::new(MockMode::Uppercase, "Italian"),
            &[segment],
            &config(),
        )
        .await
        .expect("protected span validation should preserve source after retries");

        assert_eq!(translations[0].status, SegmentStatus::NeedsReview);
        assert_eq!(translations[0].text, "Visit https://example.com");
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
