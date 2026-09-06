use super::*;
use bookforge_core::finding::QaFindingKind;
use serde::Deserialize;
use std::collections::HashSet;

/// Structured, block-attributed mirror of a batch item validation report:
/// every violation becomes one [`EngineFinding`] pinned to the item's block.
impl BatchItemValidationError {
    fn engine_findings(
        &self,
        item: &TranslationBatchItem,
        translation: &str,
    ) -> Vec<EngineFinding> {
        self.violations
            .iter()
            .map(|violation| engine_finding_for_violation(violation, item, translation))
            .collect()
    }
}

/// Map one deterministic validation violation to its canonical finding kind,
/// always attributing it to the failed item's block. Source-copy hits get
/// kind-aware severity: a block a professional translator would intentionally
/// leave unchanged (titles, headings, short reference-like lines) is a
/// warning, everything else stays an error.
fn engine_finding_for_violation(
    violation: &BatchItemValidationViolation,
    item: &TranslationBatchItem,
    translation: &str,
) -> EngineFinding {
    let message = violation.message.as_str();
    let kind = if message.starts_with("translation is unchanged from the source-language prose")
        || message.starts_with("translation retains ")
    {
        QaFindingKind::SourceCopyUnchanged
    } else if message.starts_with("protected span missing") {
        QaFindingKind::ProtectedSpanMissing
    } else if message.starts_with("inline marker missing") {
        QaFindingKind::InlineMarkerMissing
    } else if message.starts_with("inline marker duplicated") {
        QaFindingKind::InlineMarkerDuplicated
    } else if message.starts_with("unknown inline marker") {
        QaFindingKind::InlineMarkerUnknown
    } else if message.contains("marker") {
        QaFindingKind::MarkerStructure
    } else if message.contains("Toki Pona") {
        QaFindingKind::TargetLanguageGate
    } else {
        QaFindingKind::Other
    };
    let severity = if kind == QaFindingKind::SourceCopyUnchanged
        && intentionally_unchanged_block(&item.kind, &item.source_text, translation)
    {
        QaFindingSeverity::Warning
    } else {
        violation.severity
    };
    EngineFinding::new(kind, message)
        .with_block_id(item.block_id.0.clone())
        .with_severity(severity)
}

/// Blocks at most this long (after whitespace/markers normalization) can be
/// short reference-like lines: book titles, author bylines, imprint lines.
const INTENTIONALLY_UNCHANGED_MAX_CHARS: usize = 64;

/// A short block counts as "reference-like" when more than this share of its
/// word tokens are capitalized or proper-noun-ish.
const PROPER_NOUN_TOKEN_SHARE: f64 = 0.8;

/// Whether a source-copy hit on this block is plausibly intentional.
///
/// Professional translators deliberately leave some blocks unchanged when a
/// book crosses languages: the book title itself ("Cannibal Capitalism"),
/// author bylines ("Nancy Fraser"), imprint lines, and short reference-like
/// fragments. Flagging those as hard errors drowns the report in noise while
/// genuine per-block attribution is lost. A block is considered intentionally
/// unchanged when:
///
/// * its kind is `title` or `heading` (structural intent), or
/// * both the source and the returned "translation" normalize to under
///   [`INTENTIONALLY_UNCHANGED_MAX_CHARS`] chars AND more than
///   [`PROPER_NOUN_TOKEN_SHARE`] of the source's word tokens are capitalized
///   or proper-noun-ish (a title-case short line, e.g. an imprint byline).
///
/// Long prose that merely retained most of its source words never qualifies:
/// in-text English quotations embedded in translated paragraphs live in long
/// paragraph blocks and keep the error severity.
pub(crate) fn intentionally_unchanged_block(
    block_kind: &str,
    source: &str,
    translation: &str,
) -> bool {
    let kind = block_kind.trim().to_ascii_lowercase();
    if kind == "title" || kind == "heading" {
        return true;
    }
    let source_normalized = normalized_line(source);
    if source_normalized.chars().count() >= INTENTIONALLY_UNCHANGED_MAX_CHARS {
        return false;
    }
    // A short block whose "translation" ballooned into long prose is not an
    // intentional unchanged case — something else went wrong.
    if normalized_line(translation).chars().count() >= INTENTIONALLY_UNCHANGED_MAX_CHARS {
        return false;
    }
    let tokens = source_normalized
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    !tokens.is_empty()
        && (tokens
            .iter()
            .filter(|token| is_proper_noun_ish(token))
            .count() as f64)
            / (tokens.len() as f64)
            > PROPER_NOUN_TOKEN_SHARE
}

/// Whitespace-collapsed text with marker tokens stripped, for length checks.
fn normalized_line(text: &str) -> String {
    bookforge_core::marker::strip_marker_tokens(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Capitalized words, initialisms, and digit-bearing reference tokens
/// ("1987", "p. 28") all read as proper-noun-ish.
fn is_proper_noun_ish(token: &str) -> bool {
    token.chars().next().is_some_and(char::is_uppercase)
        || token.chars().all(char::is_uppercase)
        || token.chars().any(|ch| ch.is_ascii_digit())
}

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

fn prompt_projected_item(
    item: &TranslationBatchItem,
) -> (
    TranslationBatchItem,
    bookforge_core::marker::MarkerPromptProjection,
) {
    let projection = bookforge_core::marker::collapse_nested_markers_for_prompt(&item.source_text);
    let mut projected = item.clone();
    projected.source_text = projection.text.clone();
    projected
        .required_markers
        .retain(|id| !projection.is_omitted(id));
    projected.text_runs = project_marker_runs(&item.text_runs, &projection);
    (projected, projection)
}

fn project_marker_runs(
    runs: &[SegmentTextRun],
    projection: &bookforge_core::marker::MarkerPromptProjection,
) -> Vec<SegmentTextRun> {
    let mut stack = Vec::<(String, bool)>::new();
    runs.iter()
        .filter_map(|run| {
            if let Some(open) = bookforge_core::marker::parse_paired_marker_open(&run.text)
                && open.len == run.text.len()
            {
                let omitted = projection.is_omitted(&open.id);
                stack.push((open.tag_name, omitted));
                return (!omitted).then(|| run.clone());
            }
            if let Some(close) = bookforge_core::marker::parse_marker_close(&run.text)
                && close.len == run.text.len()
            {
                let omitted = stack
                    .pop()
                    .filter(|(open_name, _)| open_name == &close.tag_name)
                    .is_some_and(|(_, omitted)| omitted);
                return (!omitted).then(|| run.clone());
            }
            Some(run.clone())
        })
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
            && (alphabetic_words(&source[..start])
                .into_iter()
                .rev()
                .take(2)
                .any(is_month_name)
                || alphabetic_words(&source[end..])
                    .into_iter()
                    .take(2)
                    .any(is_month_name))
    })
}

fn alphabetic_words(text: &str) -> Vec<&str> {
    let mut words = Vec::new();
    let mut start = None;
    for (index, ch) in text.char_indices() {
        if ch.is_alphabetic() {
            start.get_or_insert(index);
        } else if let Some(word_start) = start.take() {
            words.push(&text[word_start..index]);
        }
    }
    if let Some(word_start) = start {
        words.push(&text[word_start..]);
    }
    words
}

fn is_month_name(word: &str) -> bool {
    const MONTHS: &[&str] = &[
        // English
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
        // Italian
        "gennaio",
        "gen",
        "febbraio",
        "marzo",
        "aprile",
        "maggio",
        "mag",
        "giugno",
        "giu",
        "luglio",
        "lug",
        "agosto",
        "ago",
        "settembre",
        "set",
        "ottobre",
        "ott",
        "novembre",
        "dicembre",
        "dic",
        // Spanish
        "enero",
        "ene",
        "febrero",
        "abril",
        "abr",
        "mayo",
        "junio",
        "julio",
        "septiembre",
        "setiembre",
        "octubre",
        "diciembre",
        // Portuguese
        "janeiro",
        "fevereiro",
        "fev",
        "março",
        "maio",
        "mai",
        "junho",
        "julho",
        "outubro",
        "out",
        "dezembro",
        "dez",
        // Danish and Norwegian
        "januar",
        "februar",
        "marts",
        "mars",
        "maj",
        "juni",
        "juli",
        "oktober",
        "okt",
        "desember",
        "des",
        // French
        "janvier",
        "février",
        "fevrier",
        "avril",
        "juin",
        "juillet",
        "août",
        "aout",
        "octobre",
        "décembre",
        "decembre",
        // German
        "jänner",
        "märz",
        "dezember",
    ];
    let folded = word.to_lowercase();
    MONTHS.contains(&folded.as_str())
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

/// Deserialize a batch response that was requested as bare JSON. Providers
/// occasionally wrap the payload in markdown fences or append trailing
/// commentary, which a strict parse turns into split/retry churn; when a
/// direct parse fails, fall back to parsing just the balanced outermost
/// JSON value. Only the transport wrapper is stripped — the item schema is
/// never loosened.
fn decode_batch_json<T: serde::de::DeserializeOwned>(content: &str) -> Result<T, String> {
    match serde_json::from_str(content) {
        Ok(parsed) => Ok(parsed),
        Err(direct_error) => {
            let recovered =
                outermost_json_value(content).and_then(|span| serde_json::from_str(span).ok());
            recovered.ok_or_else(|| format!("invalid batch JSON: {direct_error}"))
        }
    }
}

/// Return the balanced outermost JSON value in `content`, if any, ignoring
/// prose and markdown fencing around it. Scanning starts at the first `{`
/// or `[`, respects string literals so braces inside them are skipped, and
/// tracks a stack of open delimiters so each closer must match the most
/// recent unclosed opener.
fn outermost_json_value(content: &str) -> Option<&str> {
    let (open_index, _) = content
        .char_indices()
        .find(|(_, ch)| matches!(ch, '{' | '['))?;
    let mut open_delimiters = Vec::<char>::new();
    let mut in_string = false;
    let mut escaped = false;
    for (offset, ch) in content[open_index..].char_indices() {
        let index = open_index + offset;
        if in_string {
            match ch {
                '\\' => escaped = !escaped,
                '"' if !escaped => in_string = false,
                _ => escaped = false,
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            ch @ ('{' | '[') => {
                open_delimiters.push(if ch == '{' { '}' } else { ']' });
            }
            '}' | ']' if open_delimiters.last() == Some(&ch) => {
                open_delimiters.pop();
                if open_delimiters.is_empty() {
                    return Some(&content[open_index..=index]);
                }
            }
            _ => {}
        }
    }
    None
}

fn parse_text_batch_response(
    batch: &TranslationBatch,
    content: &str,
    turbo: bool,
    validate_source_copy: bool,
    section_titles: Option<&HashMap<String, String>>,
    target_language: Option<&str>,
) -> Result<BatchTranslationResult, String> {
    let parsed: BatchTextResponse = decode_batch_json(content)?;

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
                findings: vec![EngineFinding::new(
                    QaFindingKind::BatchBlockMismatch,
                    "duplicate item ID in batch response",
                )],
            });
            continue;
        }
        seen.insert(item.id.as_str(), ());

        let Some(request_item) = requested_ids.get(item.id.as_str()) else {
            continue;
        };

        let translation = if turbo {
            item.translation.clone()
        } else {
            let (_, projection) = prompt_projected_item(request_item);
            projection.restore(&item.translation)
        };

        if let Some(error) = crate::validation::empty_translation_validation_error(
            &request_item.source_text,
            &translation,
        ) {
            failures.push(BatchItemFailure {
                item_id: item.id.clone(),
                segment_id: request_item.segment_id.clone(),
                error: error.to_string(),
                input_tokens: None,
                input_cached_tokens: None,
                output_tokens: None,
                tokens_estimated: false,
                findings: vec![
                    EngineFinding::new(QaFindingKind::Other, error)
                        .with_block_id(request_item.block_id.0.clone()),
                ],
            });
            continue;
        }

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
            let report = validation_error.expect("validation report checked as present");
            let findings = report.engine_findings(request_item, &translation);
            let error = report.persistence_message();
            failures.push(BatchItemFailure {
                item_id: item.id.clone(),
                segment_id: request_item.segment_id.clone(),
                error,
                input_tokens: None,
                input_cached_tokens: None,
                output_tokens: None,
                tokens_estimated: false,
                findings,
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

    let missing = batch
        .items
        .iter()
        .filter(|item| !seen.contains_key(item.item_id.as_str()))
        .map(|item| item.item_id.as_str())
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(incomplete_batch_response_error(batch.items.len(), &missing));
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
    let parsed: BatchRunResponse = decode_batch_json(content)?;

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
                findings: vec![EngineFinding::new(
                    QaFindingKind::BatchBlockMismatch,
                    "duplicate item ID in batch response",
                )],
            });
            continue;
        }
        seen.insert(item.id.as_str(), ());

        let Some(request_item) = requested_ids.get(item.id.as_str()) else {
            continue;
        };
        let (projected_item, projection) = prompt_projected_item(request_item);

        let expected_run_count = projected_item.text_runs.len();
        if item.runs.len() != expected_run_count {
            let error = format!(
                "run count mismatch: expected {expected_run_count}, got {}",
                item.runs.len()
            );
            failures.push(BatchItemFailure {
                item_id: item.id.clone(),
                segment_id: request_item.segment_id.clone(),
                error: error.clone(),
                input_tokens: None,
                input_cached_tokens: None,
                output_tokens: None,
                tokens_estimated: false,
                findings: vec![
                    EngineFinding::new(QaFindingKind::MarkerStructure, error)
                        .with_block_id(request_item.block_id.0.clone()),
                ],
            });
            continue;
        }

        let expected_ids: HashMap<&str, &SegmentTextRun> = projected_item
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
            for expected in &projected_item.text_runs {
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
                error: error.clone(),
                input_tokens: None,
                input_cached_tokens: None,
                output_tokens: None,
                tokens_estimated: false,
                findings: vec![
                    EngineFinding::new(QaFindingKind::MarkerStructure, error)
                        .with_block_id(request_item.block_id.0.clone()),
                ],
            });
            continue;
        }

        let joined: Vec<String> = projected_item
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
        let translation = projection.restore(&joined_translation);
        if let Some(error) = crate::validation::empty_translation_validation_error(
            &request_item.source_text,
            &translation,
        ) {
            failures.push(BatchItemFailure {
                item_id: item.id.clone(),
                segment_id: request_item.segment_id.clone(),
                error: error.to_string(),
                input_tokens: None,
                input_cached_tokens: None,
                output_tokens: None,
                tokens_estimated: false,
                findings: vec![
                    EngineFinding::new(QaFindingKind::Other, error)
                        .with_block_id(request_item.block_id.0.clone()),
                ],
            });
            continue;
        }
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
            let report = validation_error.expect("validation report checked as present");
            let findings = report.engine_findings(request_item, &translation);
            let error = report.persistence_message();
            failures.push(BatchItemFailure {
                item_id: item.id.clone(),
                segment_id: request_item.segment_id.clone(),
                error,
                input_tokens: None,
                input_cached_tokens: None,
                output_tokens: None,
                tokens_estimated: false,
                findings,
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

    let missing = batch
        .items
        .iter()
        .filter(|item| !seen.contains_key(item.item_id.as_str()))
        .map(|item| item.item_id.as_str())
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(incomplete_batch_response_error(batch.items.len(), &missing));
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

fn incomplete_batch_response_error(requested: usize, missing: &[&str]) -> String {
    let returned = requested.saturating_sub(missing.len());
    format!(
        "batch response incomplete: requested {requested} items, returned {returned}; missing item IDs: {}",
        missing.join(", ")
    )
}

pub(super) fn batch_response_item_count(batch: &TranslationBatch, content: &str) -> Option<usize> {
    let parsed = decode_batch_json::<serde_json::Value>(content).ok()?;
    let items = parsed.get("items")?.as_array()?;
    let requested = batch
        .items
        .iter()
        .map(|item| item.item_id.as_str())
        .collect::<std::collections::HashSet<_>>();
    Some(
        items
            .iter()
            .filter_map(|item| item.get("id")?.as_str())
            .filter(|item_id| requested.contains(item_id))
            .collect::<std::collections::HashSet<_>>()
            .len(),
    )
}

fn batch_glossary_entries<'a>(
    items: &[TranslationBatchItem],
    config: &'a TranslationRunConfig,
) -> Vec<&'a bookforge_core::GlossaryPromptTerm> {
    let mut seen = HashSet::new();
    let mut entries = Vec::new();
    for item in items {
        let segment_entries = config
            .glossary
            .entries_by_segment
            .get(&item.segment_id.0)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        for entry in segment_entries {
            if seen.insert(entry) {
                entries.push(entry);
            }
        }
    }
    entries
}

pub(super) fn render_batch_glossary(
    items: &[TranslationBatchItem],
    config: &TranslationRunConfig,
) -> String {
    let entries = batch_glossary_entries(items, config);
    if entries.is_empty() {
        return String::new();
    }

    match config.glossary.format {
        GlossaryFormat::Json => {
            let rendered = serde_json::to_string(&entries).unwrap_or_else(|_| "[]".to_string());
            format!(
                "Active batch glossary constraints (must be honored throughout this batch wherever applicable):\n{rendered}"
            )
        }
        GlossaryFormat::Prose => {
            let entries = entries.into_iter().cloned().collect::<Vec<_>>();
            crate::scheduler::render_glossary_prose(&entries)
        }
    }
}

pub(super) fn render_batch_prompt_extra(
    items: &[TranslationBatchItem],
    config: &TranslationRunConfig,
) -> String {
    let mut blocks = Vec::new();
    if let Some(extra) = config
        .glossary
        .prompt_extra
        .as_deref()
        .filter(|extra| !extra.trim().is_empty())
    {
        blocks.push(extra.to_string());
    }
    let glossary = render_batch_glossary(items, config);
    if !glossary.is_empty() {
        blocks.push(glossary);
    }
    blocks.join("\n\n")
}

pub(super) fn render_batch_prompt(
    batch: &TranslationBatch,
    config: &TranslationRunConfig,
    library: &PromptLibrary,
    context_block: &str,
    compact_retry_attempt: usize,
) -> crate::prompt::Result<crate::prompt::Rendered> {
    let items_json = render_batch_items(batch, config);
    let prompt_extra = render_batch_prompt_extra(&batch.items, config);
    let template = batch_prompt_template(batch, config, library);

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
    .raw("context_translation_pairs", context_block)
    .raw("prompt_extra", prompt_extra)
    .raw("items_json", items_json);

    let mut rendered = template.render(&vars)?;
    if compact_retry_attempt > 0 {
        rendered.user.push_str(&format!(
            "\n\nRECOVERY MODE {compact_retry_attempt}: Return one compact JSON object only. Translate every item exactly once. Do not repeat any word, sentence, item, or explanation. End immediately after the closing brace."
        ));
    }
    Ok(rendered)
}

pub(super) fn batch_prompt_template<'a>(
    batch: &TranslationBatch,
    config: &TranslationRunConfig,
    library: &'a PromptLibrary,
) -> &'a crate::prompt::PromptTemplate {
    if config.compact_prompts {
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
    }
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
            let projected = (!turbo).then(|| prompt_projected_item(item).0);
            let prompt_item = projected.as_ref().unwrap_or(item);
            let source_text = if turbo {
                bookforge_core::marker::strip_marker_tokens(&prompt_item.source_text)
            } else {
                prompt_item.source_text.clone()
            };
            let required_markers = if turbo {
                Vec::new()
            } else {
                prompt_item.required_markers.clone()
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

            if let Some(guidance) = config.glossary.guidance_by_segment.get(&item.segment_id.0) {
                obj.insert(
                    "retry_guidance".to_string(),
                    serde_json::Value::String(guidance.clone()),
                );
            }

            if batch.mode == BatchMode::RunPreserving {
                let runs: Vec<serde_json::Value> = prompt_item
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
mod finding_attribution_tests {
    use super::*;
    use bookforge_core::finding::QaFindingKind;
    use bookforge_core::{
        ir::{BlockId, SectionId},
        segment::SegmentId,
    };

    fn item_with_kind(kind: &str, source: &str) -> TranslationBatchItem {
        TranslationBatchItem {
            item_id: "item".to_string(),
            segment_id: SegmentId("segment".to_string()),
            section_id: SectionId("section".to_string()),
            block_id: BlockId("block_title_001".to_string()),
            ordinal: 0,
            kind: kind.to_string(),
            source_text: source.to_string(),
            text_runs: Vec::new(),
            protected_spans: Vec::new(),
            required_markers: Vec::new(),
            checksum: "checksum".to_string(),
        }
    }

    const LONG_PROSE: &str = "This deliberately long English paragraph contains enough ordinary \
        prose to exercise untranslated-copy detection in the production response validator. It \
        repeats no special protected data and should be rejected when a provider returns it \
        unchanged instead of translating the body into the requested target language.";

    #[test]
    fn title_block_copy_hit_is_a_warning_finding_with_block_id() {
        let item = item_with_kind("title", "Cannibal Capitalism");
        let validation =
            batch_item_validation_error(&item, "Cannibal Capitalism", true, None, None)
                .expect("an unchanged title must still fail validation as before");
        assert!(
            validation.has_errors(),
            "error-string behavior is unchanged"
        );

        let findings = validation.engine_findings(&item, "Cannibal Capitalism");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, QaFindingKind::SourceCopyUnchanged);
        // Kind-aware severity: an intentionally unchanged title is editorially
        // expected, so the finding is a warning even though the legacy
        // violation (and thus the item failure) stays an error.
        assert_eq!(findings[0].severity, QaFindingSeverity::Warning);
        assert_eq!(findings[0].block_id.as_deref(), Some("block_title_001"));
    }

    #[test]
    fn prose_block_copy_hit_is_an_error_finding_with_block_id() {
        let item = item_with_kind("paragraph", LONG_PROSE);
        let validation = batch_item_validation_error(&item, LONG_PROSE, true, None, None)
            .expect("unchanged prose must fail validation");

        let findings = validation.engine_findings(&item, LONG_PROSE);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, QaFindingKind::SourceCopyUnchanged);
        assert_eq!(findings[0].severity, QaFindingSeverity::Error);
        assert_eq!(findings[0].block_id.as_deref(), Some("block_title_001"));
    }

    #[test]
    fn short_proper_noun_line_is_intentionally_unchanged_without_a_title_kind() {
        // An author byline or imprint line: short and title-case.
        assert!(intentionally_unchanged_block(
            "paragraph",
            "Nancy Fraser",
            "Nancy Fraser"
        ));
        assert!(intentionally_unchanged_block(
            "paragraph",
            "Verso Futures",
            "Verso Futures"
        ));
        // Structural kinds always qualify.
        assert!(intentionally_unchanged_block(
            "heading", "anything", "anything"
        ));
        assert!(intentionally_unchanged_block(
            "title", "anything", "anything"
        ));
        // Long prose never qualifies, even when copied.
        assert!(!intentionally_unchanged_block(
            "paragraph",
            LONG_PROSE,
            LONG_PROSE
        ));
        // Short lowercase prose is not reference-like: a genuine untranslated
        // sentence must keep its error severity.
        assert!(!intentionally_unchanged_block(
            "paragraph",
            "the quick brown fox jumps over the lazy dog",
            "the quick brown fox jumps over the lazy dog"
        ));
        // A short block whose "translation" ballooned into long prose is not
        // an intentional unchanged case.
        assert!(!intentionally_unchanged_block(
            "paragraph",
            "Nancy Fraser",
            LONG_PROSE
        ));
    }

    #[test]
    fn marker_and_protected_span_findings_are_block_attributed() {
        let mut item = item_with_kind("paragraph", "Visit https://example.com today");
        item.protected_spans = vec![ProtectedSpan {
            kind: ProtectedSpanKind::Url,
            text: "https://example.com".to_string(),
        }];
        let validation =
            batch_item_validation_error(&item, "Visita il sito oggi", true, None, None)
                .expect("a dropped protected URL must fail validation");

        let findings = validation.engine_findings(&item, "Visita il sito oggi");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, QaFindingKind::ProtectedSpanMissing);
        assert_eq!(findings[0].severity, QaFindingSeverity::Error);
        assert_eq!(findings[0].block_id.as_deref(), Some("block_title_001"));
        assert!(findings[0].message.contains("protected span missing"));
    }
}

#[cfg(test)]
mod nested_marker_projection_tests {
    use super::*;
    use bookforge_core::{
        ir::{BlockId, SectionId},
        segment::SegmentId,
    };

    fn item(source: &str, runs: Vec<SegmentTextRun>) -> TranslationBatchItem {
        TranslationBatchItem {
            item_id: "item".to_string(),
            segment_id: SegmentId("segment".to_string()),
            section_id: SectionId("section".to_string()),
            block_id: BlockId("block".to_string()),
            ordinal: 0,
            kind: "paragraph".to_string(),
            source_text: source.to_string(),
            text_runs: runs,
            protected_spans: Vec::new(),
            required_markers: bookforge_core::marker::marker_ids_in_text(source),
            checksum: "checksum".to_string(),
        }
    }

    fn batch(mode: BatchMode, item: TranslationBatchItem) -> TranslationBatch {
        TranslationBatch {
            id: "batch".to_string(),
            ordinal: 0,
            mode,
            kind: BatchKind::Translation,
            section_id: item.section_id.clone(),
            items: vec![item],
            token_estimate: 1,
        }
    }

    #[test]
    fn marker_safe_response_restores_full_source_nesting() {
        let source = "<m1><m2>eyes</m2></m1>";
        let original_item = item(source, Vec::new());
        let (projected, _) = prompt_projected_item(&original_item);

        assert_eq!(projected.source_text, "<m1>eyes</m1>");
        assert_eq!(projected.required_markers, ["m1"]);
        assert_eq!(original_item.source_text, source);
        assert_eq!(original_item.required_markers, ["m1", "m2"]);

        let result = parse_batch_response(
            &batch(BatchMode::MarkerSafe, original_item),
            r#"{"items":[{"id":"item","translation":"<m1>occhi</m1>"}]}"#,
        )
        .expect("collapsed response should parse");

        assert!(result.failures.is_empty());
        assert_eq!(result.translations[0].text, "<m1><m2>occhi</m2></m1>");
    }

    #[test]
    fn run_preserving_response_can_omit_redundant_marker_runs() {
        let source = "<m1><m2>eyes</m2></m1>";
        let runs = ["<m1>", "<m2>", "eyes", "</m2>", "</m1>"]
            .into_iter()
            .enumerate()
            .map(|(index, text)| SegmentTextRun {
                id: format!("r{index}"),
                text: text.to_string(),
            })
            .collect::<Vec<_>>();
        let original_item = item(source, runs);
        let (projected, _) = prompt_projected_item(&original_item);

        assert_eq!(
            projected
                .text_runs
                .iter()
                .map(|run| run.text.as_str())
                .collect::<Vec<_>>(),
            ["<m1>", "eyes", "</m1>"]
        );

        let result = parse_batch_response(
            &batch(BatchMode::RunPreserving, original_item),
            r#"{"items":[{"id":"item","runs":[{"id":"r0","text":"<m1>"},{"id":"r2","text":"occhi"},{"id":"r4","text":"</m1>"}]}]}"#,
        )
        .expect("projected runs should parse");

        assert!(result.failures.is_empty());
        assert_eq!(result.translations[0].text, "<m1><m2>occhi</m2></m1>");
    }
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
            ("L'evento era l'8 dicembre", "8"),
            ("El evento fue el 8 de diciembre", "8"),
            ("O evento foi em 8 de dezembro", "8"),
            ("Begivenheden var den 8. december", "8"),
            ("Arrangementet var 8. desember", "8"),
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

        assert!(
            batch_item_validation_error(
                &item_with_span("The event was December 8", ProtectedSpanKind::Number, "8"),
                "L'evento era l'8 dicembre",
                false,
                None,
                None,
            )
            .is_none(),
            "a localized date that preserves the day must pass"
        );
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

#[cfg(test)]
mod lenient_response_decoding_tests {
    use super::*;
    use bookforge_core::{
        ir::{BlockId, SectionId},
        segment::SegmentId,
    };

    fn plain_batch() -> TranslationBatch {
        let item = TranslationBatchItem {
            item_id: "item".to_string(),
            segment_id: SegmentId("segment".to_string()),
            section_id: SectionId("section".to_string()),
            block_id: BlockId("block".to_string()),
            ordinal: 0,
            kind: "paragraph".to_string(),
            source_text: "Hello".to_string(),
            text_runs: Vec::new(),
            protected_spans: Vec::new(),
            required_markers: Vec::new(),
            checksum: "checksum".to_string(),
        };
        TranslationBatch {
            id: "batch".to_string(),
            ordinal: 0,
            mode: BatchMode::Plain,
            kind: BatchKind::Translation,
            items: vec![item],
            token_estimate: 10,
            section_id: SectionId("section".to_string()),
        }
    }

    const VALID_ITEMS: &str = r#"{"items":[{"id":"item","translation":"Ciao"}]}"#;

    #[test]
    fn bare_json_still_parses_directly() {
        let result = parse_batch_response(&plain_batch(), VALID_ITEMS).expect("bare JSON parses");
        assert!(result.failures.is_empty());
        assert_eq!(result.translations[0].text, "Ciao");
    }

    #[test]
    fn markdown_fenced_json_is_accepted() {
        let fenced = format!("```json\n{VALID_ITEMS}\n```");
        let result = parse_batch_response(&plain_batch(), &fenced).expect("fenced JSON parses");
        assert!(result.failures.is_empty());
        assert_eq!(result.translations[0].text, "Ciao");
    }

    #[test]
    fn trailing_prose_after_valid_json_is_accepted() {
        let wrapped = format!("{VALID_ITEMS}\n\nEcco la traduzione richiesta!");
        let result = parse_batch_response(&plain_batch(), &wrapped)
            .expect("trailing prose must not fail the parse");
        assert!(result.failures.is_empty());
        assert_eq!(result.translations[0].text, "Ciao");
    }

    #[test]
    fn prose_before_valid_json_is_accepted() {
        let wrapped = format!("Certamente! Ecco il JSON richiesto:\n{VALID_ITEMS}");
        let result = parse_batch_response(&plain_batch(), &wrapped)
            .expect("leading prose must not fail the parse");
        assert!(result.failures.is_empty());
        assert_eq!(result.translations[0].text, "Ciao");
    }

    #[test]
    fn garbage_response_is_rejected() {
        let err = parse_batch_response(&plain_batch(), "this is not json at all").unwrap_err();
        assert!(
            err.starts_with("invalid batch JSON: "),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn braces_inside_strings_do_not_confuse_extraction() {
        // The translation itself contains braces and the payload is fenced:
        // string-aware scanning must recover the outermost value intact.
        let response = concat!(
            "```json\n",
            r#"{"items":[{"id":"item","translation":"{braced} }"}]}"#,
            "\n```\nFatto."
        );
        let result = parse_batch_response(&plain_batch(), response).expect("nested braces parse");
        assert!(result.failures.is_empty());
        assert_eq!(result.translations[0].text, "{braced} }");
    }

    #[test]
    fn unterminated_json_is_rejected() {
        let err = parse_batch_response(&plain_batch(), "{\"items\":[{\"id\":\"item\"").unwrap_err();
        assert!(
            err.starts_with("invalid batch JSON: "),
            "unexpected error: {err}"
        );
    }
}
