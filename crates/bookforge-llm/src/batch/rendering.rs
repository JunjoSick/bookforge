use super::*;
use serde::Deserialize;

pub(super) fn batch_item_validation_error(
    item: &TranslationBatchItem,
    translation: &str,
    validate_source_copy: bool,
    section_title: Option<&str>,
    target_language: Option<&str>,
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
    if let Some(error) = target_language.and_then(|target_language| {
        crate::validation::target_language_validation_error(
            target_language,
            &item.source_text,
            translation,
            &item.protected_spans,
        )
    }) {
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
    parse_batch_response_with_validation(batch, response_json, false, None, None)
}

pub(super) fn parse_batch_response_with_validation(
    batch: &TranslationBatch,
    response_json: &str,
    validate_source_copy: bool,
    section_titles: Option<&HashMap<String, String>>,
    target_language: Option<&str>,
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
                target_language,
            )
        }
        BatchMode::RunPreserving => parse_run_batch_response(
            batch,
            content,
            validate_source_copy,
            section_titles,
            target_language,
        ),
    }
}

fn parse_text_batch_response(
    batch: &TranslationBatch,
    content: &str,
    turbo: bool,
    validate_source_copy: bool,
    section_titles: Option<&HashMap<String, String>>,
    target_language: Option<&str>,
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
            request_item
                .protected_spans
                .iter()
                .find(|span| {
                    !span.trim().is_empty()
                        && !crate::validation::protected_span_present(span, &translation)
                })
                .map(|span| format!("protected span missing: {span}"))
                .or_else(|| {
                    source_copy_error(
                        request_item,
                        &translation,
                        validate_source_copy,
                        section_title,
                    )
                })
                .or_else(|| {
                    target_language.and_then(|target_language| {
                        crate::validation::target_language_validation_error(
                            target_language,
                            &request_item.source_text,
                            &translation,
                            &request_item.protected_spans,
                        )
                    })
                })
        } else {
            batch_item_validation_error(
                request_item,
                &translation,
                validate_source_copy,
                section_title,
                target_language,
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

        let translation = if turbo {
            wrap_text_only_translation_with_source_markers(&request_item.source_text, &translation)
        } else {
            translation
        };
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
    target_language: Option<&str>,
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
        let joined_translation = joined.join("");
        let translation = joined_translation;
        let section_title = section_titles
            .and_then(|titles| titles.get(&request_item.segment_id.0))
            .map(String::as_str);
        if let Some(error) = batch_item_validation_error(
            request_item,
            &translation,
            validate_source_copy,
            section_title,
            target_language,
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

pub(super) fn render_batch_items(
    batch: &TranslationBatch,
    config: &TranslationRunConfig,
) -> String {
    let items: Vec<serde_json::Value> = batch
        .items
        .iter()
        .map(|item| {
            let turbo = batch.mode == BatchMode::TurboTextOnly;
            let source_text = if turbo {
                bookforge_core::marker::strip_marker_tokens(&item.source_text)
            } else {
                item.source_text.clone()
            };
            let required_markers = if turbo {
                Vec::new()
            } else {
                item.required_markers.clone()
            };
            let protected_spans = item.protected_spans.clone();
            let mut obj = serde_json::json!({
                "id": item.item_id,
                "kind": item.kind,
                "text": source_text,
                "required_markers": required_markers,
                "protected": protected_spans,
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

fn wrap_text_only_translation_with_source_markers(source: &str, translation: &str) -> String {
    let mut prefix = String::new();
    let mut rest = source;
    while let Some(index) = rest.find('<') {
        let tag = &rest[index..];
        if let Some(open) = bookforge_core::marker::parse_paired_marker_open(tag) {
            prefix.push_str(&tag[..open.len]);
            prefix.push_str(&format!("</{}>", open.tag_name));
            rest = &tag[open.len..];
        } else if let Some(empty) = bookforge_core::marker::parse_empty_marker(tag) {
            prefix.push_str(&tag[..empty.len]);
            rest = &tag[empty.len..];
        } else if let Some(close) = bookforge_core::marker::parse_marker_close(tag) {
            rest = &tag[close.len..];
        } else {
            rest = &tag[1..];
        }
    }
    let translation = bookforge_core::marker::strip_marker_tokens(translation);
    if prefix.is_empty() {
        return translation;
    }
    format!("{prefix}{translation}")
}

#[cfg(test)]
mod text_only_marker_tests {
    use super::wrap_text_only_translation_with_source_markers;

    #[test]
    fn text_only_retry_preserves_markers_without_applying_source_formatting_to_all_text() {
        let wrapped = wrap_text_only_translation_with_source_markers(
            "Before <m1>bold</m1> and <m2>italic <m3>nested</m3></m2>.",
            "toki pona pi lipu ni",
        );

        assert_eq!(
            bookforge_core::marker::marker_ids_in_text(&wrapped),
            ["m1", "m2", "m3"]
        );
        assert!(bookforge_core::marker::marker_structure_error(&wrapped).is_none());
        assert_eq!(wrapped, "<m1></m1><m2></m2><m3></m3>toki pona pi lipu ni");
    }

    #[test]
    fn preserves_empty_source_markers_once() {
        let wrapped = wrap_text_only_translation_with_source_markers(
            "Text <ref id=\"r1\"/> after",
            "toki sin",
        );

        assert_eq!(bookforge_core::marker::marker_ids_in_text(&wrapped), ["r1"]);
        assert!(bookforge_core::marker::marker_structure_error(&wrapped).is_none());
    }

    #[test]
    fn strips_model_supplied_markers_before_restoring_source_template() {
        let wrapped = wrap_text_only_translation_with_source_markers(
            "Text <m1>bold</m1>",
            "<m1>toki pona</m1>",
        );

        assert_eq!(wrapped, "<m1></m1>toki pona");
        assert_eq!(bookforge_core::marker::marker_ids_in_text(&wrapped), ["m1"]);
    }
}
