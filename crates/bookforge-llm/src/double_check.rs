use bookforge_core::{
    config::DoubleCheckConfig,
    ir::BlockId,
    segment::{Segment, SegmentId, SegmentStatus},
};
use serde::Deserialize;

use crate::{
    CompletionRequest, LlmError, LlmProvider, PromptLibrary, RequestMetadata, ResponseFormat,
    SegmentTranslation, Substitutions, TranslationRunConfig,
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
    for translation in translations {
        let Some(segment) = by_segment.get(translation.segment_id.0.as_str()) else {
            continue;
        };
        if translation.status != SegmentStatus::Succeeded {
            continue;
        }

        for (i, block_t) in translation.blocks.iter().enumerate() {
            let block = segment.source.blocks.get(i);
            items.push(DoubleCheckItem {
                id: format!("{}:{}", segment.id.0, block_t.block_id.0),
                segment_id: segment.id.0.clone(),
                block_id: block_t.block_id.0.clone(),
                section_title: segment.metadata.section_title.clone(),
                kind: block.map(|b| b.kind.clone()).unwrap_or_default(),
                source: block.map(|b| b.text.clone()).unwrap_or_default(),
                translation: block_t.text.clone(),
                required_markers: segment.constraints.preserve_markers.clone(),
                protected_spans: block.map(|b| b.protected_spans.clone()).unwrap_or_default(),
                deterministic_warnings: Vec::new(),
            });
        }
    }

    let chunk_size = double_check_config.batch_target_tokens.max(1);
    let chunks: Vec<Vec<DoubleCheckItem>> = items
        .chunks(chunk_size.max(1))
        .map(|c| c.to_vec())
        .collect();

    let mut all_issues = Vec::new();
    for chunk in &chunks {
        let audit_result =
            run_audit_chunk(&provider, library, chunk, config, double_check_config).await?;
        all_issues.extend(audit_result);
    }

    if !double_check_config.auto_correct {
        let records: Vec<CorrectionRecord> = all_issues
            .into_iter()
            .map(|item| CorrectionRecord {
                item_id: item.item_id,
                original_translation: item.current_translation,
                corrected_translation: None,
                status: CorrectionStatus::Unresolved,
                issues: item.issues,
            })
            .collect();
        return Ok(records);
    }

    let correction_items: Vec<CorrectionItem> = all_issues
        .into_iter()
        .filter(|item| item.issues.iter().any(|i| i.needs_correction))
        .collect();

    let mut records = Vec::new();
    for corr_chunk in correction_items.chunks(chunk_size.max(1)) {
        let corr_results = run_correction_chunk(&provider, library, corr_chunk, config).await?;

        for result in corr_results {
            let valid = validate_correction(&result);
            records.push(CorrectionRecord {
                item_id: result.item_id.clone(),
                original_translation: result.current_translation.clone(),
                corrected_translation: Some(result.current_translation.clone()),
                status: valid,
                issues: result.issues.clone(),
            });
        }
    }

    Ok(records)
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

    let response = provider
        .complete(CompletionRequest {
            system: rendered.system,
            user: rendered.user,
            response_format: ResponseFormat::Json,
            temperature: 0.0,
            max_output_tokens: None,
            metadata: RequestMetadata::default(),
        })
        .await?;

    let parsed: DoubleCheckResponse = serde_json::from_str(&response.content)?;

    let mut corrections = Vec::new();
    let item_map: std::collections::HashMap<&str, &DoubleCheckItem> =
        items.iter().map(|item| (item.id.as_str(), item)).collect();

    for result in &parsed.items {
        let Some(source_item) = item_map.get(result.id.as_str()) else {
            continue;
        };
        if result.verdict == "pass" && result.issues.is_empty() {
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

    let response = provider
        .complete(CompletionRequest {
            system: rendered.system,
            user: rendered.user,
            response_format: ResponseFormat::Json,
            temperature: 0.1,
            max_output_tokens: None,
            metadata: RequestMetadata::default(),
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

fn validate_correction(item: &CorrectionItem) -> CorrectionStatus {
    let text = &item.current_translation;

    if text.is_empty() && !item.source.is_empty() {
        return CorrectionStatus::RejectedValidationFailed(
            "corrected translation is empty".to_string(),
        );
    }

    for marker in &item.required_markers {
        if !text.contains(marker) {
            return CorrectionStatus::RejectedValidationFailed(format!(
                "missing required marker: {marker}"
            ));
        }
    }

    for span in &item.protected_spans {
        if !text.contains(span) {
            return CorrectionStatus::RejectedValidationFailed(format!(
                "missing protected span: {span}"
            ));
        }
    }

    CorrectionStatus::Applied
}

fn double_check_mode_str(mode: bookforge_core::config::DoubleCheckMode) -> &'static str {
    match mode {
        bookforge_core::config::DoubleCheckMode::Off => "off",
        bookforge_core::config::DoubleCheckMode::Formatting => "formatting",
        bookforge_core::config::DoubleCheckMode::Semantic => "semantic",
        bookforge_core::config::DoubleCheckMode::Full => "full",
    }
}
