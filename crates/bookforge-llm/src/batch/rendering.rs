use super::*;
use serde::Deserialize;

pub fn batch_item_validation_error(
    item: &TranslationBatchItem,
    translation: &str,
    validate_source_copy: bool,
    section_title: Option<&str>,
    target_language: Option<&str>,
) -> Option<BatchItemValidationError> {
    let mut violations = Vec::new();
    if let Some(error) = bookforge_core::marker::marker_structure_error(translation) {
        violations.push(hard_violation(error));
    }
    let expected = bookforge_core::marker::marker_ids_in_text(&item.source_text);
    let actual = bookforge_core::marker::marker_ids_in_text(translation);
    for marker in &expected {
        let count = actual.iter().filter(|found| *found == marker).count();
        if count == 0 {
            violations.push(hard_violation(format!("inline marker missing: {marker}")));
        }
        if count > 1 {
            violations.push(hard_violation(format!(
                "inline marker duplicated: {marker}"
            )));
        }
    }
    for marker in &actual {
        if !expected.contains(marker) {
            violations.push(hard_violation(format!("unknown inline marker: {marker}")));
        }
    }
    if let Some(error) =
        crate::validation::marker_reference_text_error(&item.source_text, translation)
    {
        violations.push(hard_violation(error));
    }
    violations.extend(protected_span_violations(item, translation));
    if let Some(error) = source_copy_error(item, translation, validate_source_copy, section_title) {
        violations.push(hard_violation(error));
    }
    let protected_spans = protected_span_texts(item);
    if let Some(error) = target_language.and_then(|target_language| {
        crate::validation::target_language_validation_error(
            target_language,
            &item.source_text,
            translation,
            &protected_spans,
        )
    }) {
        violations.push(hard_violation(error));
    }
    BatchItemValidationError::new(violations)
}

fn turbo_batch_item_validation_error(
    item: &TranslationBatchItem,
    translation: &str,
    validate_source_copy: bool,
    section_title: Option<&str>,
    target_language: Option<&str>,
) -> Option<BatchItemValidationError> {
    let mut violations = protected_span_violations(item, translation);
    if let Some(error) = source_copy_error(item, translation, validate_source_copy, section_title) {
        violations.push(hard_violation(error));
    }
    let protected_spans = protected_span_texts(item);
    if let Some(error) = target_language.and_then(|target_language| {
        crate::validation::target_language_validation_error(
            target_language,
            &item.source_text,
            translation,
            &protected_spans,
        )
    }) {
        violations.push(hard_violation(error));
    }
    BatchItemValidationError::new(violations)
}

fn hard_violation(message: String) -> BatchItemValidationViolation {
    BatchItemValidationViolation {
        severity: QaFindingSeverity::Error,
        protected_span_kind: None,
        message,
    }
}

fn protected_span_violations(
    item: &TranslationBatchItem,
    translation: &str,
) -> Vec<BatchItemValidationViolation> {
    item.protected_spans
        .iter()
        .filter(|span| {
            !span.text.trim().is_empty()
                && !crate::validation::protected_span_present(&span.text, translation)
        })
        .map(|span| {
            let severity = protected_span_severity(item, span.kind, &span.text);
            let kind_suffix = format!(" [kind={}]", span.kind.as_str());
            BatchItemValidationViolation {
                severity,
                protected_span_kind: Some(span.kind),
                message: format!("protected span missing: {}{kind_suffix}", span.text),
            }
        })
        .collect()
}

fn protected_span_texts(item: &TranslationBatchItem) -> Vec<String> {
    item.protected_spans
        .iter()
        .map(|span| span.text.clone())
        .collect()
}

fn protected_span_severity(
    item: &TranslationBatchItem,
    kind: ProtectedSpanKind,
    text: &str,
) -> QaFindingSeverity {
    match kind {
        ProtectedSpanKind::Math => QaFindingSeverity::Warning,
        ProtectedSpanKind::Number => number_violation_severity(item, text),
        ProtectedSpanKind::Url
        | ProtectedSpanKind::Email
        | ProtectedSpanKind::Code
        | ProtectedSpanKind::Filename
        | ProtectedSpanKind::InternalAnchor
        | ProtectedSpanKind::Citation
        | ProtectedSpanKind::FootnoteReference => QaFindingSeverity::Error,
    }
}

fn number_violation_severity(item: &TranslationBatchItem, text: &str) -> QaFindingSeverity {
    let digit_count = text.chars().filter(char::is_ascii_digit).count();
    let has_currency = text
        .chars()
        .any(|ch| matches!(ch, '$' | '\u{20ac}' | '\u{00a3}' | '\u{00a5}'));
    let chars = text.chars().collect::<Vec<_>>();
    let has_decimal = chars.windows(3).any(|window| {
        window[0].is_ascii_digit() && matches!(window[1], '.' | ',') && window[2].is_ascii_digit()
    });
    let numeric_components = text
        .split(|ch: char| !ch.is_ascii_digit())
        .filter(|component| !component.is_empty())
        .count();
    let bare_small_integer =
        (1..=2).contains(&digit_count) && text.chars().all(|ch| ch.is_ascii_digit());
    let critical_small_integer = bare_small_integer
        && (item.kind == "list_item"
            || starts_numbered_item(&item.source_text, text)
            || number_adjacent_to_month(&item.source_text, text));

    if digit_count >= 3
        || has_currency
        || has_decimal
        || numeric_components >= 2
        || critical_small_integer
    {
        QaFindingSeverity::Error
    } else {
        QaFindingSeverity::Warning
    }
}

fn starts_numbered_item(source: &str, number: &str) -> bool {
    let source = bookforge_core::marker::strip_marker_tokens(source);
    let Some(rest) = source.trim_start().strip_prefix(number) else {
        return false;
    };
    rest.chars()
        .next()
        .is_some_and(|ch| matches!(ch, '.' | ')' | ':' | ']'))
}

fn number_adjacent_to_month(source: &str, number: &str) -> bool {
    source.match_indices(number).any(|(start, _)| {
        let end = start + number.len();
        let left_boundary = source[..start]
            .chars()
            .next_back()
            .is_none_or(|ch| !ch.is_ascii_digit());
        let right_boundary = source[end..]
            .chars()
            .next()
            .is_none_or(|ch| !ch.is_ascii_digit());
        left_boundary
            && right_boundary
            && (adjacent_word_before(source, start).is_some_and(is_month_name)
                || adjacent_word_after(source, end).is_some_and(is_month_name))
    })
}

fn adjacent_word_before(source: &str, end: usize) -> Option<&str> {
    let prefix = &source[..end];
    let word_end = prefix
        .char_indices()
        .rev()
        .find(|(_, ch)| ch.is_ascii_alphabetic())
        .map(|(index, ch)| index + ch.len_utf8())?;
    let word_start = prefix[..word_end]
        .char_indices()
        .rev()
        .take_while(|(_, ch)| ch.is_ascii_alphabetic())
        .last()
        .map_or(word_end, |(index, _)| index);
    Some(&prefix[word_start..word_end])
}

fn adjacent_word_after(source: &str, start: usize) -> Option<&str> {
    let suffix = &source[start..];
    let word_start = suffix
        .char_indices()
        .find(|(_, ch)| ch.is_ascii_alphabetic())
        .map(|(index, _)| index)?;
    let word_end = suffix[word_start..]
        .char_indices()
        .take_while(|(_, ch)| ch.is_ascii_alphabetic())
        .last()
        .map(|(index, ch)| word_start + index + ch.len_utf8())?;
    Some(&suffix[word_start..word_end])
}

fn is_month_name(word: &str) -> bool {
    const MONTHS: &[&str] = &[
        "january",
        "jan",
        "february",
        "feb",
        "march",
        "mar",
        "april",
        "apr",
        "may",
        "june",
        "jun",
        "july",
        "jul",
        "august",
        "aug",
        "september",
        "sep",
        "sept",
        "october",
        "oct",
        "november",
        "nov",
        "december",
        "dec",
    ];
    MONTHS.iter().any(|month| word.eq_ignore_ascii_case(month))
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
            turbo_batch_item_validation_error(
                request_item,
                &translation,
                validate_source_copy,
                section_title,
                target_language,
            )
        } else {
            batch_item_validation_error(
                request_item,
                &translation,
                validate_source_copy,
                section_title,
                target_language,
            )
        };
        if validation_error
            .as_ref()
            .is_some_and(BatchItemValidationError::has_errors)
        {
            let error = validation_error
                .expect("validation report checked as present")
                .persistence_message();
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
            warning: validation_error.map(|report| report.persistence_message()),
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
        let validation_error = batch_item_validation_error(
            request_item,
            &translation,
            validate_source_copy,
            section_title,
            target_language,
        );
        if validation_error
            .as_ref()
            .is_some_and(BatchItemValidationError::has_errors)
        {
            let error = validation_error
                .expect("validation report checked as present")
                .persistence_message();
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
            warning: validation_error.map(|report| report.persistence_message()),
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
            let protected_spans = protected_span_texts(item);
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
mod protected_span_severity_tests {
    use super::*;
    use bookforge_core::{
        ir::{BlockId, ProtectedSpan, ProtectedSpanKind, SectionId},
        segment::SegmentId,
    };

    fn item_with_span(source: &str, kind: ProtectedSpanKind, text: &str) -> TranslationBatchItem {
        TranslationBatchItem {
            item_id: "item".to_string(),
            segment_id: SegmentId("segment".to_string()),
            section_id: SectionId("section".to_string()),
            block_id: BlockId("block".to_string()),
            ordinal: 0,
            kind: "paragraph".to_string(),
            source_text: source.to_string(),
            text_runs: Vec::new(),
            protected_spans: vec![ProtectedSpan {
                kind,
                text: text.to_string(),
            }],
            required_markers: Vec::new(),
            checksum: "checksum".to_string(),
        }
    }

    fn parse_one(item: TranslationBatchItem, translation: &str) -> BatchTranslationResult {
        let batch = TranslationBatch {
            id: "batch".to_string(),
            ordinal: 0,
            mode: item.mode(),
            kind: BatchKind::Translation,
            items: vec![item],
            token_estimate: 10,
            section_id: SectionId("section".to_string()),
        };
        let response = serde_json::json!({
            "items": [{"id": "item", "translation": translation}]
        })
        .to_string();
        parse_batch_response(&batch, &response).expect("batch response parses")
    }

    #[test]
    fn missing_math_is_a_successful_item_with_warning() {
        let result = parse_one(
            item_with_span("Einstein wrote E=mc^2", ProtectedSpanKind::Math, "E=mc^2"),
            "Einstein scrisse la formula",
        );

        assert!(result.failures.is_empty());
        assert_eq!(result.translations.len(), 1);
        assert!(
            result.translations[0]
                .warning
                .as_deref()
                .is_some_and(|warning| {
                    warning.contains("warning: protected span missing: E=mc^2")
                })
        );
    }

    #[test]
    fn missing_url_remains_a_hard_item_failure() {
        let result = parse_one(
            item_with_span(
                "Visit https://example.com",
                ProtectedSpanKind::Url,
                "https://example.com",
            ),
            "Visita il sito",
        );

        assert!(result.translations.is_empty());
        assert_eq!(result.failures.len(), 1);
        assert!(
            result.failures[0]
                .error
                .contains("error: protected span missing: https://example.com")
        );
    }

    #[test]
    fn hard_marker_violation_is_not_masked_by_soft_math() {
        let mut item = item_with_span("<m1>E=mc^2</m1>", ProtectedSpanKind::Math, "E=mc^2");
        item.required_markers = vec!["m1".to_string()];
        let validation = batch_item_validation_error(&item, "La formula", false, None, None)
            .expect("both violations should be reported");

        assert!(validation.has_errors());
        assert_eq!(validation.violations().len(), 2);
        assert!(validation.contains("inline marker missing: m1"));
        assert!(validation.contains("protected span missing: E=mc^2"));
        assert!(validation.violations().iter().any(|violation| {
            violation.severity == QaFindingSeverity::Warning
                && violation.protected_span_kind == Some(ProtectedSpanKind::Math)
        }));
    }

    #[test]
    fn number_severity_uses_substance_and_critical_context_boundary() {
        let weak = batch_item_validation_error(
            &item_with_span("Chapter 42 explains it", ProtectedSpanKind::Number, "42"),
            "Il capitolo lo spiega",
            false,
            None,
            None,
        )
        .expect("missing weak number is still surfaced");
        assert!(!weak.has_errors());

        for (source, number) in [
            ("Published in 1987", "1987"),
            ("The value is 0.0027", "0.0027"),
            ("It cost $15", "$15"),
            ("The event was December 8", "8"),
            ("1. First item", "1"),
        ] {
            let substantial = batch_item_validation_error(
                &item_with_span(source, ProtectedSpanKind::Number, number),
                "La traduzione omette il dato",
                false,
                None,
                None,
            )
            .expect("missing number is surfaced");
            assert!(
                substantial.has_errors(),
                "{number} in {source:?} should be hard: {substantial}"
            );
        }
    }
}

#[cfg(test)]
mod noteref_marker_validation_tests {
    use super::*;

    /// `word.<sup><a epub:type="noteref">*2</a></sup>` reaches the model as
    /// `word.<m0><m1>*2</m1></m0>`. Every marker ID survives when the model
    /// drops the `*2`, so only the inner-text check can catch it.
    fn noteref_item() -> TranslationBatchItem {
        TranslationBatchItem {
            item_id: "item_noteref".to_string(),
            segment_id: SegmentId("seg_noteref".to_string()),
            section_id: bookforge_core::ir::SectionId("section_noteref".to_string()),
            block_id: bookforge_core::ir::BlockId("block_noteref".to_string()),
            ordinal: 0,
            kind: "paragraph".to_string(),
            source_text: "The word.<m0><m1>*2</m1></m0> It follows.".to_string(),
            text_runs: Vec::new(),
            protected_spans: Vec::new(),
            required_markers: vec!["m0".to_string(), "m1".to_string()],
            checksum: "checksum_noteref".to_string(),
        }
    }

    #[test]
    fn dropped_noteref_token_fails_batch_item_validation() {
        let error = batch_item_validation_error(
            &noteref_item(),
            "La parola.<m0><m1></m1></m0> Segue.",
            false,
            None,
            None,
        )
        .expect("a dropped endnote reference must fail validation");

        assert!(error.contains("lost its reference text"), "got: {error}");
        assert!(error.contains("*2"), "got: {error}");
    }

    #[test]
    fn preserved_noteref_token_passes_batch_item_validation() {
        assert_eq!(
            batch_item_validation_error(
                &noteref_item(),
                "La parola.<m0><m1>*2</m1></m0> Segue.",
                false,
                None,
                None,
            ),
            None
        );
    }

    #[test]
    fn translated_marker_prose_still_passes_batch_item_validation() {
        let mut item = noteref_item();
        item.source_text = "A <m0>beautiful</m0> day.".to_string();
        item.required_markers = vec!["m0".to_string()];

        assert_eq!(
            batch_item_validation_error(
                &item,
                "Una <m0>bellissima</m0> giornata.",
                false,
                None,
                None
            ),
            None
        );
    }
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
