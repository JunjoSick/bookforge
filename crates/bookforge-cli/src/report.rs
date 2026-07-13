use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use anyhow::Result;
use bookforge_core::segment::Segment;
use bookforge_llm::QaSegmentReview;
use bookforge_store::{JobRecord, JobSummary, SegmentRecord};
use serde::Serialize;

use crate::cost::estimate_cost_usd_with_cached;
use crate::performance::RunPerformanceSummary;

#[derive(Debug, Clone)]
pub(crate) struct ReportFiles {
    pub json: PathBuf,
    pub markdown: PathBuf,
}

/// Decoupled view of a segment's translated text used to drive the QA
/// heuristics in [`qa_warnings`]. Kept independent of both
/// `bookforge_llm::SegmentTranslation` (live in-memory run results) and
/// `bookforge_store::StoredSegmentTranslation` (reloaded from the database)
/// so the report can be built fresh from either source — in particular, so a
/// manual correction (which only has store-backed data available) can
/// regenerate the report without pulling in a full in-memory run result.
#[derive(Debug, Clone)]
pub(crate) struct TranslationQaInput {
    pub segment_id: String,
    /// Whether this translation is in a terminal "counts as translated"
    /// state (mirrors `SegmentStatus::Succeeded | SegmentStatus::SkippedCached`).
    pub counts_for_warnings: bool,
    pub joined_text: String,
}

impl TranslationQaInput {
    pub(crate) fn new(
        segment_id: impl Into<String>,
        counts_for_warnings: bool,
        joined_text: impl Into<String>,
    ) -> Self {
        Self {
            segment_id: segment_id.into(),
            counts_for_warnings,
            joined_text: joined_text.into(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct ReportInput<'a> {
    pub job: &'a JobRecord,
    pub summary: &'a JobSummary,
    pub segments: &'a [Segment],
    pub segment_records: &'a [SegmentRecord],
    pub translations: &'a [TranslationQaInput],
    pub qa_reviews: &'a [QaSegmentReview],
    pub performance: Option<RunPerformanceSummary>,
    pub output: &'a Path,
    /// Count of segments whose stored translation carries a human
    /// (manual-correction) origin. Additive counterpart to the other
    /// aggregate segment counts below — kept in sync with `review.rs`'s
    /// per-segment `human_corrected` field.
    pub corrected_segments: usize,
}

#[derive(Debug, Serialize)]
struct QaReport {
    job_id: String,
    status: String,
    provider: String,
    model: String,
    source_language: Option<String>,
    target_language: String,
    output: String,
    total_segments: usize,
    successful_segments: usize,
    cached_segments: usize,
    retried_segments: usize,
    failed_segments: usize,
    needs_review_segments: usize,
    retry_pending_segments: usize,
    corrected_segments: usize,
    input_tokens: u64,
    input_cached_tokens: u64,
    output_tokens: u64,
    estimated_cost: Option<f64>,
    qa_reviewed_segments: usize,
    qa_warnings: Vec<QaWarning>,
    performance: Option<RunPerformanceSummary>,
}

#[derive(Debug, Clone, Serialize)]
struct QaWarning {
    severity: &'static str,
    kind: &'static str,
    segment_id: Option<String>,
    message: String,
}

pub(crate) fn write_report(input: ReportInput<'_>) -> Result<ReportFiles> {
    let files = report_paths(input.output);
    let report = QaReport {
        job_id: input.job.id.clone(),
        status: input.summary.status.clone(),
        provider: input.job.provider.clone(),
        model: input.job.model.clone(),
        source_language: input.job.source_lang.clone(),
        target_language: input.job.target_lang.clone(),
        output: input.output.display().to_string(),
        total_segments: input.summary.total_segments,
        successful_segments: input.summary.succeeded,
        cached_segments: input.summary.cached,
        retried_segments: input.summary.retried,
        failed_segments: input.summary.failed,
        needs_review_segments: input.summary.needs_review,
        retry_pending_segments: input.summary.retry_pending,
        corrected_segments: input.corrected_segments,
        input_tokens: input.summary.input_tokens,
        input_cached_tokens: input.summary.input_cached_tokens,
        output_tokens: input.summary.output_tokens,
        estimated_cost: estimate_cost_usd_with_cached(
            &input.job.provider,
            &input.job.model,
            input.summary.input_tokens,
            input.summary.input_cached_tokens,
            input.summary.output_tokens,
        ),
        qa_reviewed_segments: input.qa_reviews.len(),
        qa_warnings: qa_warnings(&input),
        performance: input.performance.clone(),
    };

    if let Some(parent) = files.json.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&files.json, serde_json::to_string_pretty(&report)?)?;
    fs::write(&files.markdown, render_markdown(&report))?;
    Ok(files)
}

pub(crate) fn report_paths(output: &Path) -> ReportFiles {
    let parent = output.parent().unwrap_or_else(|| Path::new(""));
    let stem = output
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("book");
    ReportFiles {
        json: parent.join(format!("{stem}.report.json")),
        markdown: parent.join(format!("{stem}.report.md")),
    }
}

fn qa_warnings(input: &ReportInput<'_>) -> Vec<QaWarning> {
    let mut warnings = Vec::new();
    let mut seen = BTreeSet::<(String, &'static str)>::new();

    for record in input.segment_records {
        match record.status.as_str() {
            "failed" => warnings.push(QaWarning {
                severity: "error",
                kind: "failed_segment",
                segment_id: Some(record.id.clone()),
                message: record
                    .error
                    .clone()
                    .unwrap_or_else(|| "segment failed without a stored error".to_string()),
            }),
            "needs_review" => warnings.push(QaWarning {
                severity: "warning",
                kind: "needs_review",
                segment_id: Some(record.id.clone()),
                message: record
                    .error
                    .clone()
                    .unwrap_or_else(|| "segment requires review".to_string()),
            }),
            "retry_pending" => warnings.push(QaWarning {
                severity: "warning",
                kind: "retry_pending",
                segment_id: Some(record.id.clone()),
                message: "segment is still pending retry".to_string(),
            }),
            _ => {}
        }
    }

    let source_by_segment = input
        .segments
        .iter()
        .map(|segment| (segment.id.0.as_str(), segment.source.text.as_str()))
        .collect::<BTreeMap<_, _>>();

    for translation in input.translations {
        if !translation.counts_for_warnings {
            continue;
        }
        let Some(source) = source_by_segment.get(translation.segment_id.as_str()) else {
            continue;
        };
        let translated = translation.joined_text.clone();
        let source_len = source.chars().count().max(1);
        let translated_len = translated.chars().count();
        if source_len >= 40 {
            let ratio = translated_len as f64 / source_len as f64;
            if !(0.33..=3.0).contains(&ratio)
                && seen.insert((translation.segment_id.clone(), "length_ratio"))
            {
                warnings.push(QaWarning {
                    severity: "warning",
                    kind: "length_ratio",
                    segment_id: Some(translation.segment_id.clone()),
                    message: format!(
                        "translated length ratio is suspicious: {ratio:.2} ({source_len} source chars, {translated_len} target chars)"
                    ),
                });
            }
        }

        if source_len >= 40
            && source.trim() == translated.trim()
            && seen.insert((translation.segment_id.clone(), "untranslated"))
        {
            warnings.push(QaWarning {
                severity: "warning",
                kind: "untranslated",
                segment_id: Some(translation.segment_id.clone()),
                message: "translation is identical to the source text".to_string(),
            });
        }

        if let Some(message) = missing_tokens_message("URL", &urls(source), &urls(&translated))
            && seen.insert((translation.segment_id.clone(), "url_changed"))
        {
            warnings.push(QaWarning {
                severity: "warning",
                kind: "url_changed",
                segment_id: Some(translation.segment_id.clone()),
                message,
            });
        }

        if let Some(message) =
            missing_tokens_message("number", &numbers(source), &numbers(&translated))
            && seen.insert((translation.segment_id.clone(), "number_changed"))
        {
            warnings.push(QaWarning {
                severity: "warning",
                kind: "number_changed",
                segment_id: Some(translation.segment_id.clone()),
                message,
            });
        }

        if looks_like_model_commentary(&translated)
            && seen.insert((translation.segment_id.clone(), "model_commentary"))
        {
            warnings.push(QaWarning {
                severity: "warning",
                kind: "model_commentary",
                segment_id: Some(translation.segment_id.clone()),
                message: "translation appears to include model commentary".to_string(),
            });
        }

        if has_repetition(&translated)
            && seen.insert((translation.segment_id.clone(), "repetition"))
        {
            warnings.push(QaWarning {
                severity: "warning",
                kind: "repetition",
                segment_id: Some(translation.segment_id.clone()),
                message: "translation contains suspicious repeated words".to_string(),
            });
        }
    }

    for review in input.qa_reviews {
        if review.verdict == "pass" && review.issues.is_empty() {
            continue;
        }
        let severity = if review.verdict == "fail" {
            "error"
        } else {
            "warning"
        };
        if review.issues.is_empty() {
            warnings.push(QaWarning {
                severity,
                kind: "qa_review",
                segment_id: Some(review.segment_id.0.clone()),
                message: format!("QA verdict: {}", review.verdict),
            });
        } else {
            for issue in &review.issues {
                warnings.push(QaWarning {
                    severity,
                    kind: "qa_review",
                    segment_id: Some(review.segment_id.0.clone()),
                    message: format!(
                        "{} [{}]: {}{}{}",
                        issue.severity,
                        issue.kind,
                        issue.message,
                        issue
                            .source_excerpt
                            .as_ref()
                            .map(|text| format!(" source={text:?}"))
                            .unwrap_or_default(),
                        issue
                            .translation_excerpt
                            .as_ref()
                            .map(|text| format!(" translation={text:?}"))
                            .unwrap_or_default()
                    ),
                });
            }
        }
    }

    warnings
}

pub(crate) fn urls(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter_map(|token| {
            let value = token.trim_matches(|ch: char| {
                matches!(
                    ch,
                    ',' | ';' | ':' | '.' | '!' | '?' | ')' | ']' | '"' | '\''
                )
            });
            (value.starts_with("http://") || value.starts_with("https://"))
                .then(|| value.to_string())
        })
        .collect()
}

pub(crate) fn numbers(text: &str) -> Vec<String> {
    let chars = text.chars().collect::<Vec<_>>();
    let mut out = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        if !is_number_start(&chars, index) {
            index += 1;
            continue;
        }
        let start = index;
        index += 1;
        while index < chars.len() && is_number_body(chars[index]) {
            index += 1;
        }
        let value = trim_number_token(&chars[start..index]);
        let digits = value.chars().filter(|ch| ch.is_ascii_digit()).count();
        if digits >= 2 {
            out.push(value);
        }
    }
    out
}

fn is_number_start(chars: &[char], index: usize) -> bool {
    chars[index].is_ascii_digit()
        || (matches!(chars[index], '$' | '+' | '-' | '−' | '–' | '—')
            && chars
                .get(index + 1)
                .is_some_and(|next| next.is_ascii_digit()))
}

fn is_number_body(ch: char) -> bool {
    ch.is_ascii_digit()
        || matches!(
            ch,
            '.' | ',' | ':' | '/' | '-' | '+' | '%' | '$' | '−' | '–' | '—'
        )
}

fn trim_number_token(chars: &[char]) -> String {
    chars
        .iter()
        .collect::<String>()
        .trim()
        .trim_matches(|ch: char| {
            matches!(
                ch,
                ',' | ';'
                    | ':'
                    | '.'
                    | '!'
                    | '?'
                    | '('
                    | ')'
                    | '['
                    | ']'
                    | '{'
                    | '}'
                    | '"'
                    | '\''
                    | '“'
                    | '”'
                    | '‘'
                    | '’'
                    | '«'
                    | '»'
            )
        })
        .to_string()
}

fn missing_tokens_message(label: &str, source: &[String], translated: &[String]) -> Option<String> {
    let missing = source
        .iter()
        .filter(|token| !token_present(token, translated))
        .cloned()
        .collect::<Vec<_>>();
    (!missing.is_empty()).then(|| format!("missing preserved {label}(s): {}", missing.join(", ")))
}

pub(crate) fn token_present(token: &str, translated: &[String]) -> bool {
    dangling_numeric_span(token)
        || translated.iter().any(|candidate| candidate == token)
        || compact_numeric_punctuation_span(token).is_some_and(|expected| {
            compact_ascii_whitespace(&translated.join("")).contains(&expected)
        })
        || digits_only_numeric_punctuation_span(token)
            .is_some_and(|expected| digits_only(&translated.join("")).contains(&expected))
        || canonical_decimal_number(token).is_some_and(|expected| {
            translated
                .iter()
                .any(|candidate| canonical_decimal_number(candidate).as_deref() == Some(&expected))
        })
}

fn dangling_numeric_span(value: &str) -> bool {
    let normalized = normalize_number_signs(value);
    let trimmed = normalized.trim_matches(|ch: char| {
        matches!(
            ch,
            ',' | ';' | ':' | '.' | '!' | '?' | '(' | ')' | '[' | ']' | '"' | '\''
        )
    });
    trimmed.ends_with('-') && trimmed.chars().any(|ch| ch.is_ascii_digit())
}

fn digits_only_numeric_punctuation_span(value: &str) -> Option<String> {
    compact_numeric_punctuation_span(value).map(|compact| digits_only(&compact))
}

fn digits_only(value: &str) -> String {
    value.chars().filter(|ch| ch.is_ascii_digit()).collect()
}

fn compact_numeric_punctuation_span(value: &str) -> Option<String> {
    let normalized = normalize_number_signs(value);
    let trimmed = normalized.trim_matches(|ch: char| {
        matches!(
            ch,
            ',' | ';' | ':' | '.' | '!' | '?' | '(' | ')' | '[' | ']' | '"' | '\''
        )
    });
    let digits = trimmed.chars().filter(|ch| ch.is_ascii_digit()).count();
    if digits < 2 {
        return None;
    }
    if !trimmed.chars().all(|ch| {
        ch.is_ascii_digit()
            || ch.is_ascii_whitespace()
            || matches!(
                ch,
                '.' | ',' | ';' | ':' | '/' | '-' | '+' | '%' | '$' | '(' | ')'
            )
    }) {
        return None;
    }
    Some(compact_ascii_whitespace(trimmed))
}

fn compact_ascii_whitespace(value: &str) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace())
        .collect()
}

fn canonical_decimal_number(value: &str) -> Option<String> {
    let normalized = normalize_number_signs(value);
    let trimmed = normalized.trim_matches(|ch: char| {
        matches!(
            ch,
            ',' | ';' | ':' | '.' | '!' | '?' | '(' | ')' | '[' | ']' | '"' | '\''
        )
    });
    if trimmed.is_empty() || !trimmed.chars().any(|ch| ch.is_ascii_digit()) {
        return None;
    }
    let percent = trimmed.ends_with('%');
    let numeric = trimmed.strip_suffix('%').unwrap_or(trimmed);
    if !numeric
        .chars()
        .all(|ch| ch.is_ascii_digit() || matches!(ch, '.' | ',' | '-' | '+'))
    {
        return None;
    }
    if numeric.matches('.').count() + numeric.matches(',').count() > 1 {
        return None;
    }
    let separator = numeric.find('.').or_else(|| numeric.find(','));
    let mut canonical = match separator {
        Some(index) => {
            let (whole, fractional_with_separator) = numeric.split_at(index);
            let fractional = &fractional_with_separator[1..];
            if whole.is_empty()
                || fractional.is_empty()
                || !whole
                    .trim_start_matches(['-', '+'])
                    .chars()
                    .all(|ch| ch.is_ascii_digit())
                || !fractional.chars().all(|ch| ch.is_ascii_digit())
            {
                return None;
            }
            format!("{whole}.{fractional}")
        }
        None => numeric.to_string(),
    };
    if percent {
        canonical.push('%');
    }
    Some(canonical)
}

fn normalize_number_signs(value: &str) -> String {
    value.chars().map(normalize_number_sign).collect()
}

fn normalize_number_sign(ch: char) -> char {
    match ch {
        '−' | '–' | '—' => '-',
        _ => ch,
    }
}

pub(crate) fn looks_like_model_commentary(text: &str) -> bool {
    let lower = text.trim_start().to_ascii_lowercase();
    lower.starts_with("here is ")
        || lower.starts_with("here's ")
        || lower.starts_with("certainly")
        || lower.starts_with("translation:")
        || lower.contains("as an ai")
}

fn has_repetition(text: &str) -> bool {
    let words = text
        .split_whitespace()
        .map(|word| {
            word.trim_matches(|ch: char| !ch.is_ascii_alphanumeric())
                .to_ascii_lowercase()
        })
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    words
        .windows(4)
        .any(|window| window[0] == window[1] && window[1] == window[2] && window[2] == window[3])
}

fn render_markdown(report: &QaReport) -> String {
    let mut output = String::new();
    output.push_str("# Bookforge QA Report\n\n");
    output.push_str(&format!("- Job: `{}`\n", report.job_id));
    output.push_str(&format!("- Status: `{}`\n", report.status));
    output.push_str(&format!("- Provider: `{}`\n", report.provider));
    output.push_str(&format!("- Model: `{}`\n", report.model));
    output.push_str(&format!(
        "- Target language: `{}`\n",
        report.target_language
    ));
    output.push_str(&format!("- Output: `{}`\n\n", report.output));

    output.push_str("## Summary\n\n");
    output.push_str(&format!(
        "- Translated: {}/{} segments\n",
        report.successful_segments, report.total_segments
    ));
    output.push_str(&format!("- Cached: {}\n", report.cached_segments));
    output.push_str(&format!("- Retried: {}\n", report.retried_segments));
    output.push_str(&format!(
        "- Needs review: {}\n",
        report.needs_review_segments
    ));
    output.push_str(&format!("- Failed: {}\n", report.failed_segments));
    output.push_str(&format!(
        "- Retry pending: {}\n",
        report.retry_pending_segments
    ));
    output.push_str(&format!(
        "- Manually corrected: {}\n",
        report.corrected_segments
    ));
    output.push_str(&format!("- Input tokens: {}\n", report.input_tokens));
    output.push_str(&format!(
        "- Cached input tokens: {}\n",
        report.input_cached_tokens
    ));
    output.push_str(&format!("- Output tokens: {}\n", report.output_tokens));
    output.push_str(&format!(
        "- QA reviewed segments: {}\n",
        report.qa_reviewed_segments
    ));
    match report.estimated_cost {
        Some(cost) => output.push_str(&format!("- Estimated cost: ${cost:.6}\n\n")),
        None => output.push_str("- Estimated cost: not available\n\n"),
    }

    output.push_str("## QA Warnings\n\n");
    if report.qa_warnings.is_empty() {
        output.push_str("No QA warnings.\n");
    } else {
        for warning in &report.qa_warnings {
            let segment = warning.segment_id.as_deref().unwrap_or("job");
            output.push_str(&format!(
                "- **{}** `{}` `{}`: {}\n",
                warning.severity, warning.kind, segment, warning.message
            ));
        }
    }

    output.push_str("\n## Performance\n\n");
    if let Some(perf) = &report.performance {
        output.push_str(&format!("- Requests: {}\n", perf.request_count));
        output.push_str(&format!(
            "- Latency p50/p95: {}/{} ms\n",
            optional_u64(perf.p50_latency_ms),
            optional_u64(perf.p95_latency_ms)
        ));
        output.push_str(&format!("- Retries: {}\n", perf.retries));
        output.push_str(&format!(
            "- 429/timeouts/server errors: {}/{}/{}\n",
            perf.rate_limited, perf.timeouts, perf.server_errors
        ));
        output.push_str(&format!(
            "- Invalid responses/truncations: {}/{}\n",
            perf.invalid_responses, perf.truncations
        ));
        output.push_str(&format!(
            "- Batch splits/repair batches/repair failures: {}/{}/{}\n",
            perf.batch_splits, perf.repair_batches, perf.repair_failures
        ));
        output.push_str(&format!(
            "- Checkpoint flushes: {}\n",
            perf.checkpoint_flushes
        ));
        output.push_str(&format!(
            "- Blocks/min: {}\n",
            perf.blocks_per_minute
                .map(|value| format!("{value:.2}"))
                .unwrap_or_else(|| "n/a".to_string())
        ));
    } else {
        output.push_str("Performance data unavailable: no event log was available.\n");
    }

    output
}

fn optional_u64(value: Option<u64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "n/a".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_number_message_accepts_decimal_comma_localization() {
        let source = numbers("diameter 0.1 mm and potential -63.5 mV");
        let translated = numbers("diametro 0,1 mm e potenziale –63,5 mV");

        assert!(missing_tokens_message("number", &source, &translated).is_none());
    }

    #[test]
    fn missing_number_message_accepts_localized_thousands_separators() {
        let source = numbers("including 400,000 members and 112,000 co-judges");
        let translated = numbers("compresi 400.000 membri e 112.000 co-giudici");

        assert!(missing_tokens_message("number", &source, &translated).is_none());
    }

    #[test]
    fn missing_number_message_finds_numbers_attached_to_quotes_or_elisions() {
        let source = numbers("from $50,000 to 80.3 percent");
        let translated = numbers("da «$50,000» all'80,3 per cento");

        assert!(missing_tokens_message("number", &source, &translated).is_none());
    }

    #[test]
    fn missing_number_message_accepts_citation_spacing() {
        let source = numbers("Skou (1957,1989) isolated an ATPase");
        let translated = numbers("Skou (1957, 1989) isolò una ATPasi");

        assert!(missing_tokens_message("number", &source, &translated).is_none());
    }

    #[test]
    fn missing_number_message_accepts_localized_date_without_english_comma() {
        let source = numbers("official act on November 8, 1917");
        let translated = numbers("atto ufficiale l'8 novembre 1917");

        assert!(missing_tokens_message("number", &source, &translated).is_none());
    }

    #[test]
    fn missing_number_message_ignores_dangling_numeric_artifacts() {
        let source = numbers("The value was 10-");
        let translated = numbers("Il valore era 7,3 × 10⁻⁷");

        assert!(missing_tokens_message("number", &source, &translated).is_none());
    }

    #[test]
    fn missing_number_message_still_reports_absent_numbers() {
        let source = numbers("diameter 0.1 mm and potential -63.5 mV");
        let translated = numbers("diametro 0,1 mm");

        let message = missing_tokens_message("number", &source, &translated)
            .expect("missing number should be reported");

        assert!(message.contains("-63.5"));
    }
}
