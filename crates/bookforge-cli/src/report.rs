use bookforge_core::numeric::{
    canonical_decimal_number, compact_ascii_whitespace, compact_numeric_punctuation_span,
    dangling_numeric_span,
};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs,
    path::{Path, PathBuf},
};

use anyhow::Result;
use bookforge_core::finding::findings_from_legacy_error_text;
use bookforge_core::segment::{SEGMENT_UNIT_NAME, Segment};
use bookforge_llm::{QaIssue, QaSegmentReview};
use bookforge_store::{
    JobRecord, JobStore, JobSummary, QaFinding, QaFindingCount, QaFindingSeverity, SegmentRecord,
    StoredQaFinding, aggregate_findings,
};
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
    /// Structured QA findings persisted for this job (from the store's
    /// `qa_findings` table). These are the preferred source for the finding
    /// breakdown: they carry block attribution and the per-instance severity
    /// recorded at checkpoint time.
    pub qa_findings: Vec<StoredQaFinding>,
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
    segment_unit: &'static str,
    total_segments: usize,
    successful_segments: usize,
    cached_segments: usize,
    retried_segments: usize,
    failed_segments: usize,
    needs_review_segments: usize,
    needs_review_rate_percent: f64,
    retry_pending_segments: usize,
    corrected_segments: usize,
    input_tokens: u64,
    input_cached_tokens: u64,
    output_tokens: u64,
    estimated_cost: Option<f64>,
    qa_reviewed_segments: usize,
    qa_warnings: Vec<QaWarning>,
    finding_breakdown: Vec<QaFindingBreakdownEntry>,
    performance: Option<RunPerformanceSummary>,
}

#[derive(Debug, Clone, Serialize)]
struct QaFindingBreakdownEntry {
    kind: String,
    severity: String,
    count: usize,
    share_percent: f64,
}

#[derive(Debug, Clone, Serialize)]
struct QaWarning {
    severity: &'static str,
    kind: &'static str,
    segment_id: Option<String>,
    message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CollapsedQaIssue {
    pub segment_ids: Vec<String>,
    pub severity: &'static str,
    pub kind: String,
    pub message: String,
    pub source_excerpt: Option<String>,
    pub translation_excerpts: Vec<(String, String)>,
    pub occurrence_count: usize,
}

impl CollapsedQaIssue {
    pub(crate) fn representative_segment_id(&self) -> &str {
        self.segment_ids
            .first()
            .map(String::as_str)
            .unwrap_or("unknown")
    }

    pub(crate) fn stored_severity(&self) -> &'static str {
        if self.severity == "high" {
            "error"
        } else {
            "warning"
        }
    }

    pub(crate) fn formatted_message(&self) -> String {
        let mut message = format!("{} [{}]: {}", self.severity, self.kind, self.message);
        if let Some(source_excerpt) = &self.source_excerpt {
            message.push_str(&format!(" source={source_excerpt:?}"));
        }
        if !self.translation_excerpts.is_empty() {
            let translations = self
                .translation_excerpts
                .iter()
                .map(|(segment_id, excerpt)| format!("{segment_id}: {excerpt:?}"))
                .collect::<Vec<_>>()
                .join(", ");
            message.push_str(&format!(" translations=[{translations}]"));
        }
        message.push_str(&format!(
            " occurrences={} segments=[{}]",
            self.occurrence_count,
            self.segment_ids.join(", ")
        ));
        message
    }
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
        segment_unit: SEGMENT_UNIT_NAME,
        total_segments: input.summary.total_segments,
        successful_segments: input.summary.succeeded,
        cached_segments: input.summary.cached,
        retried_segments: input.summary.retried,
        failed_segments: input.summary.failed,
        needs_review_segments: input.summary.needs_review,
        needs_review_rate_percent: segment_share_percent(
            input.summary.needs_review,
            input.summary.total_segments,
        ),
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
        finding_breakdown: finding_breakdown(input.segment_records, &input.qa_findings),
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

/// Per-kind classification of every flagged segment in this run.
///
/// `qa_warnings` above lists each flag individually, which buries the shape of
/// the problem once a book produces hundreds of them. This rolls the same
/// failures up by kind so a single misfiring validator is obvious at the top of
/// the list.
///
/// Structured rows from the store (`qa_findings`, persisted at checkpoint
/// time) are the preferred source: they keep block attribution and the
/// per-instance severity the checkpoint recorded — a source-copy hit on a
/// section title stays a warning instead of the legacy "70% error" display.
/// Only flagged segments with no structured rows fall back to decomposing
/// their stored `segments.error` string, and that decomposition goes through
/// the shared core parser (`findings_from_legacy_error_text`) rather than a
/// CLI-local duplicate. LLM-review rows keep their own `llm_*` namespace and
/// stay out of this deterministic rollup. `status` consumes the same counts,
/// so the report and `bookforge status` cannot drift apart.
pub(crate) fn finding_breakdown_counts(
    records: &[SegmentRecord],
    qa_findings: &[StoredQaFinding],
) -> Vec<QaFindingCount> {
    let flagged: HashSet<&str> = records
        .iter()
        .filter(|record| matches!(record.status.as_str(), "failed" | "needs_review"))
        .map(|record| record.id.as_str())
        .collect();
    if flagged.is_empty() {
        return Vec::new();
    }

    let mut structured = Vec::new();
    let mut segments_with_rows = HashSet::new();
    for finding in qa_findings {
        if finding.kind.starts_with("llm_") || !flagged.contains(finding.segment_id.as_str()) {
            continue;
        }
        segments_with_rows.insert(finding.segment_id.as_str());
        structured.push(QaFinding {
            kind: finding.finding_kind(),
            severity: finding_severity(&finding.severity),
            message: finding.message.clone(),
            block_id: finding.block_id.clone(),
        });
    }

    let legacy = records
        .iter()
        .filter(|record| {
            flagged.contains(record.id.as_str()) && !segments_with_rows.contains(record.id.as_str())
        })
        .filter_map(|record| record.error.as_deref())
        .flat_map(findings_from_legacy_error_text)
        .map(QaFinding::from);

    aggregate_findings(structured.into_iter().chain(legacy))
}

/// The report's breakdown view of the shared counts (same rows, plus each
/// kind's share of the total).
fn finding_breakdown(
    records: &[SegmentRecord],
    qa_findings: &[StoredQaFinding],
) -> Vec<QaFindingBreakdownEntry> {
    let counts = finding_breakdown_counts(records, qa_findings);
    let total = counts.iter().map(|count| count.count).sum::<usize>();
    counts
        .iter()
        .map(|count| QaFindingBreakdownEntry {
            kind: count.kind.clone(),
            severity: count.severity.clone(),
            count: count.count,
            share_percent: count.share_percent(total),
        })
        .collect()
}

fn finding_severity(value: &str) -> QaFindingSeverity {
    match value {
        "warning" => QaFindingSeverity::Warning,
        _ => QaFindingSeverity::Error,
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

    for review in input
        .qa_reviews
        .iter()
        .filter(|review| review.issues.is_empty())
    {
        if review.verdict == "pass" {
            continue;
        }
        let severity = if review.verdict == "fail" {
            "error"
        } else {
            "warning"
        };
        warnings.push(QaWarning {
            severity,
            kind: "qa_review",
            segment_id: Some(review.segment_id.0.clone()),
            message: format!("QA verdict: {}", review.verdict),
        });
    }

    for issue in collapse_qa_issues(input.qa_reviews) {
        warnings.push(QaWarning {
            severity: issue.stored_severity(),
            kind: "qa_review",
            segment_id: Some(issue.representative_segment_id().to_string()),
            message: issue.formatted_message(),
        });
    }

    warnings
}

pub(crate) fn collapse_qa_issues(reviews: &[QaSegmentReview]) -> Vec<CollapsedQaIssue> {
    let mut collapsed = Vec::<CollapsedQaIssue>::new();
    let mut merge_targets = std::collections::HashMap::<(String, String), usize>::new();

    for review in reviews {
        for issue in &review.issues {
            let kind = normalize_issue_kind(&issue.kind);
            let normalized_source = issue
                .source_excerpt
                .as_deref()
                .map(normalize_source_excerpt)
                .filter(|excerpt| !excerpt.is_empty());
            let merge_target = normalized_source
                .as_ref()
                .and_then(|source| merge_targets.get(&(kind.clone(), source.clone())).copied());

            if let Some(index) = merge_target {
                merge_collapsed_issue(&mut collapsed[index], review, issue);
                continue;
            }

            let index = collapsed.len();
            let source_excerpt = issue
                .source_excerpt
                .as_deref()
                .map(str::trim)
                .filter(|excerpt| !excerpt.is_empty())
                .map(ToString::to_string);
            let mut translation_excerpts = Vec::new();
            if let Some(excerpt) = nonempty_excerpt(issue.translation_excerpt.as_deref()) {
                translation_excerpts.push((review.segment_id.0.clone(), excerpt.to_string()));
            }
            collapsed.push(CollapsedQaIssue {
                segment_ids: vec![review.segment_id.0.clone()],
                severity: normalize_issue_severity(&issue.severity),
                kind: kind.clone(),
                message: issue.message.clone(),
                source_excerpt,
                translation_excerpts,
                occurrence_count: 1,
            });
            if let Some(source) = normalized_source {
                merge_targets.insert((kind, source), index);
            }
        }
    }

    collapsed
}

pub(crate) fn persist_qa_reviews_best_effort(
    store: &JobStore,
    job_id: &str,
    reviews: &[QaSegmentReview],
) {
    let collapsed = collapse_qa_issues(reviews);
    let messages = collapsed
        .iter()
        .map(CollapsedQaIssue::formatted_message)
        .collect::<Vec<_>>();
    let findings = collapsed
        .iter()
        .zip(&messages)
        .map(|(issue, message)| {
            (
                issue.representative_segment_id(),
                issue.kind.as_str(),
                issue.severity,
                message.as_str(),
            )
        })
        .collect::<Vec<_>>();

    if let Err(error) = store.replace_llm_qa_findings(job_id, &findings) {
        eprintln!("warning: could not persist LLM QA findings for job '{job_id}': {error}");
    }
}

fn merge_collapsed_issue(
    collapsed: &mut CollapsedQaIssue,
    review: &QaSegmentReview,
    issue: &QaIssue,
) {
    collapsed.occurrence_count += 1;
    if !collapsed.segment_ids.contains(&review.segment_id.0) {
        collapsed.segment_ids.push(review.segment_id.0.clone());
    }
    let severity = normalize_issue_severity(&issue.severity);
    if issue_severity_rank(severity) > issue_severity_rank(collapsed.severity) {
        collapsed.severity = severity;
    }
    if let Some(excerpt) = nonempty_excerpt(issue.translation_excerpt.as_deref()) {
        let occurrence = (review.segment_id.0.clone(), excerpt.to_string());
        if !collapsed.translation_excerpts.contains(&occurrence) {
            collapsed.translation_excerpts.push(occurrence);
        }
    }
}

fn normalize_issue_kind(kind: &str) -> String {
    let mut normalized = String::new();
    let mut last_was_separator = false;
    for character in kind.trim().chars().flat_map(char::to_lowercase) {
        if character.is_alphanumeric() {
            normalized.push(character);
            last_was_separator = false;
        } else if !normalized.is_empty() && !last_was_separator {
            normalized.push('_');
            last_was_separator = true;
        }
    }
    while normalized.ends_with('_') {
        normalized.pop();
    }
    if normalized.is_empty() {
        "other".to_string()
    } else {
        normalized
    }
}

fn normalize_source_excerpt(excerpt: &str) -> String {
    excerpt
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn normalize_issue_severity(severity: &str) -> &'static str {
    match severity.trim().to_ascii_lowercase().as_str() {
        "high" => "high",
        "low" => "low",
        _ => "medium",
    }
}

fn issue_severity_rank(severity: &str) -> u8 {
    match severity {
        "high" => 2,
        "medium" => 1,
        _ => 0,
    }
}

fn nonempty_excerpt(excerpt: Option<&str>) -> Option<&str> {
    excerpt.map(str::trim).filter(|excerpt| !excerpt.is_empty())
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

fn digits_only_numeric_punctuation_span(value: &str) -> Option<String> {
    compact_numeric_punctuation_span(value).map(|compact| digits_only(&compact))
}

fn digits_only(value: &str) -> String {
    value.chars().filter(|ch| ch.is_ascii_digit()).collect()
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
        "- Translated: {}/{} scheduler segments\n",
        report.successful_segments, report.total_segments
    ));
    output.push_str(&format!("- Cached: {}\n", report.cached_segments));
    output.push_str(&format!("- Retried: {}\n", report.retried_segments));
    output.push_str(&format!(
        "- Needs review: {}/{} scheduler segments ({:.1}%)\n",
        report.needs_review_segments, report.total_segments, report.needs_review_rate_percent
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

    output.push_str("\n## Flag Breakdown\n\n");
    if report.finding_breakdown.is_empty() {
        output.push_str("No flagged segments.\n");
    } else {
        for entry in &report.finding_breakdown {
            output.push_str(&format!(
                "- `{}` ({}): {} ({:.1}%)\n",
                entry.kind, entry.severity, entry.count, entry.share_percent
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

fn segment_share_percent(count: usize, total: usize) -> f64 {
    if total == 0 {
        0.0
    } else {
        (count as f64 * 1_000.0 / total as f64).round() / 10.0
    }
}

fn optional_u64(value: Option<u64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "n/a".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static REPORT_TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn qa_review(
        segment_id: &str,
        kind: &str,
        source_excerpt: Option<&str>,
        translation_excerpt: Option<&str>,
    ) -> QaSegmentReview {
        QaSegmentReview {
            segment_id: bookforge_core::segment::SegmentId(segment_id.to_string()),
            verdict: "warn".to_string(),
            issues: vec![QaIssue {
                severity: "medium".to_string(),
                kind: kind.to_string(),
                message: "The title is translated inconsistently".to_string(),
                source_excerpt: source_excerpt.map(ToString::to_string),
                translation_excerpt: translation_excerpt.map(ToString::to_string),
            }],
        }
    }

    fn segment_record(id: &str, status: &str, error: Option<&str>) -> SegmentRecord {
        SegmentRecord {
            id: id.to_string(),
            status: status.to_string(),
            attempts: 1,
            error: error.map(ToString::to_string),
            input_tokens: None,
            input_cached_tokens: None,
            output_tokens: None,
            tokens_estimated: false,
        }
    }

    #[test]
    fn finding_breakdown_falls_back_to_core_parser_without_structured_rows() {
        let records = [
            segment_record(
                "seg_0",
                "needs_review",
                Some("protected span missing from segment 'seg_0': 4th"),
            ),
            segment_record(
                "seg_1",
                "needs_review",
                Some("protected span missing from segment 'seg_1': 5.16"),
            ),
            segment_record(
                "seg_2",
                "failed",
                Some("translation checkpoint failure: provider error: HTTP status 503: down"),
            ),
            // Succeeded segments never contribute, even if a stale error string
            // is still attached to the record.
            segment_record("seg_3", "succeeded", Some("protected span missing: x")),
        ];

        let breakdown = finding_breakdown(&records, &[]);

        assert_eq!(breakdown.len(), 2);
        assert_eq!(breakdown[0].kind, "protected_span_missing");
        assert_eq!(breakdown[0].count, 2);
        assert_eq!(breakdown[0].share_percent, 66.7);
        assert_eq!(breakdown[1].kind, "other");
        assert_eq!(breakdown[1].count, 1);
    }

    #[test]
    fn finding_breakdown_prefers_structured_rows_with_instance_severity() {
        let records = [
            segment_record(
                "seg_0",
                "needs_review",
                Some("error: translation is unchanged from the source-language prose"),
            ),
            segment_record("seg_1", "failed", Some("provider error: HTTP 500")),
        ];
        // Structured rows as persisted at checkpoint time: the source-copy
        // hit carries its instance warning severity, and the raw error string
        // is never re-parsed for this segment.
        let stored = vec![
            StoredQaFinding {
                id: "qaf_a".to_string(),
                segment_id: "seg_0".to_string(),
                kind: "source_copy_unchanged".to_string(),
                severity: "warning".to_string(),
                message: "title block copied unchanged".to_string(),
                block_id: Some("b_000001".to_string()),
            },
            StoredQaFinding {
                id: "qaf_b".to_string(),
                segment_id: "seg_1".to_string(),
                kind: "provider_error".to_string(),
                severity: "error".to_string(),
                message: "provider error: HTTP 500".to_string(),
                block_id: None,
            },
        ];

        let breakdown = finding_breakdown(&records, &stored);

        assert_eq!(breakdown.len(), 2);
        let source_copy = breakdown
            .iter()
            .find(|entry| entry.kind == "source_copy_unchanged")
            .expect("structured source copy entry");
        assert_eq!(source_copy.severity, "warning", "instance severity wins");
        assert_eq!(source_copy.count, 1);
        let provider = breakdown
            .iter()
            .find(|entry| entry.kind == "provider_error")
            .expect("structured provider entry");
        assert_eq!(provider.severity, "error");
        // No duplicate rows from re-parsing the legacy strings.
        assert!(breakdown.iter().all(|entry| entry.count == 1));
    }

    #[test]
    fn finding_breakdown_merges_structured_and_legacy_rows() {
        let records = [
            // seg_0 has a structured row; seg_1 only a legacy error string.
            segment_record("seg_0", "needs_review", Some("unparsed legacy text")),
            segment_record(
                "seg_1",
                "failed",
                Some("protected span missing from segment 'seg_1': 4th"),
            ),
        ];
        let stored = vec![StoredQaFinding {
            id: "qaf_a".to_string(),
            segment_id: "seg_0".to_string(),
            kind: "inline_marker_missing".to_string(),
            severity: "error".to_string(),
            message: "inline marker missing: m1".to_string(),
            block_id: None,
        }];

        let breakdown = finding_breakdown(&records, &stored);

        assert_eq!(
            breakdown
                .iter()
                .map(|entry| entry.kind.as_str())
                .collect::<Vec<_>>(),
            vec!["inline_marker_missing", "protected_span_missing"]
        );
    }

    #[test]
    fn finding_breakdown_counts_each_failure_in_a_concatenated_error() {
        let records = [segment_record(
            "seg_0",
            "needs_review",
            Some(
                "translation is unchanged from the source-language prose; \
                 batch translation block mismatch: missing=[\"b_000853\"], extra=[], duplicate=[]",
            ),
        )];

        let breakdown = finding_breakdown(&records, &[]);

        assert_eq!(
            breakdown
                .iter()
                .map(|entry| entry.kind.as_str())
                .collect::<Vec<_>>(),
            vec!["batch_block_mismatch", "source_copy_unchanged"]
        );
        assert!(breakdown.iter().all(|entry| entry.count == 1));
        // The legacy source-copy hit is a warning at the instance level.
        let source_copy = breakdown
            .iter()
            .find(|entry| entry.kind == "source_copy_unchanged")
            .expect("source copy entry");
        assert_eq!(source_copy.severity, "warning");
    }

    #[test]
    fn finding_breakdown_is_empty_without_flagged_segments() {
        assert!(finding_breakdown(&[segment_record("seg_0", "succeeded", None)], &[]).is_empty());
    }

    #[test]
    fn qa_findings_collapse_by_kind_and_normalized_source_excerpt() {
        let mut reviews = (0..9)
            .map(|index| {
                qa_review(
                    &format!("seg_{index}"),
                    "mistranslation",
                    Some("  The   Cyberiad "),
                    Some(if index % 2 == 0 {
                        "Il Ciberiade"
                    } else {
                        "La Ciberiade"
                    }),
                )
            })
            .collect::<Vec<_>>();
        reviews.push(qa_review(
            "seg_other_1",
            "mistranslation",
            Some("cyberspace"),
            Some("il ciberspazio"),
        ));
        reviews.push(qa_review(
            "seg_other_2",
            "mistranslation",
            Some("cybernetics"),
            Some("la cibernetica"),
        ));

        let collapsed = collapse_qa_issues(&reviews);

        assert_eq!(collapsed.len(), 3);
        assert_eq!(collapsed[0].occurrence_count, 9);
        assert_eq!(collapsed[0].segment_ids.len(), 9);
        assert_eq!(
            collapsed[0].segment_ids,
            (0..9)
                .map(|index| format!("seg_{index}"))
                .collect::<Vec<_>>()
        );
        assert_eq!(collapsed[1].occurrence_count, 1);
        assert_eq!(collapsed[2].occurrence_count, 1);
        let message = collapsed[0].formatted_message();
        assert!(message.contains("occurrences=9"));
        for index in 0..9 {
            assert!(message.contains(&format!("seg_{index}")));
        }
    }

    #[test]
    fn qa_findings_without_source_excerpts_do_not_merge() {
        let reviews = [
            qa_review("seg_0", "mistranslation", None, Some("uno")),
            qa_review("seg_1", "mistranslation", None, Some("due")),
        ];

        let collapsed = collapse_qa_issues(&reviews);

        assert_eq!(collapsed.len(), 2);
        assert!(collapsed.iter().all(|issue| issue.occurrence_count == 1));
    }

    #[test]
    fn report_keeps_reviewed_segment_count_while_collapsing_qa_warnings() {
        let reviews = (0..9)
            .map(|index| {
                qa_review(
                    &format!("seg_{index}"),
                    "mistranslation",
                    Some("The Cyberiad"),
                    Some("La Ciberiade"),
                )
            })
            .collect::<Vec<_>>();
        let job = JobRecord {
            id: "job_report_qa".to_string(),
            input_path: PathBuf::from("source.epub"),
            input_snapshot_path: None,
            input_sha256: None,
            output_path: PathBuf::from("target.epub"),
            input_hash: "hash".to_string(),
            source_lang: Some("English".to_string()),
            target_lang: "Italian".to_string(),
            provider: "mock".to_string(),
            model: "mock".to_string(),
            base_url: None,
            api_key_env: None,
            status: "succeeded".to_string(),
            events_path: None,
            report_json_path: None,
            report_markdown_path: None,
            book_id: None,
            series_id: None,
        };
        let summary = JobSummary {
            id: job.id.clone(),
            status: job.status.clone(),
            total_segments: 9,
            succeeded: 9,
            ..JobSummary::default()
        };
        let output = std::env::temp_dir().join(format!(
            "bookforge-qa-report-{}-{}.epub",
            std::process::id(),
            REPORT_TEST_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));

        let files = write_report(ReportInput {
            job: &job,
            summary: &summary,
            segments: &[],
            segment_records: &[],
            translations: &[],
            qa_reviews: &reviews,
            qa_findings: Vec::new(),
            performance: None,
            output: &output,
            corrected_segments: 0,
        })
        .expect("report writes");
        let report: serde_json::Value =
            serde_json::from_slice(&fs::read(&files.json).expect("JSON report can be read"))
                .expect("JSON report parses");

        assert_eq!(report["qa_reviewed_segments"], 9);
        let warnings = report["qa_warnings"]
            .as_array()
            .expect("qa_warnings stays an array");
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0]["kind"], "qa_review");
        assert!(
            warnings[0]["message"]
                .as_str()
                .is_some_and(|message| message.contains("occurrences=9"))
        );

        let _ = fs::remove_file(files.json);
        let _ = fs::remove_file(files.markdown);
    }

    #[test]
    fn needs_review_rate_uses_scheduler_segment_denominator() {
        assert_eq!(segment_share_percent(8, 32), 25.0);
        assert_eq!(segment_share_percent(1, 3), 33.3);
        assert_eq!(segment_share_percent(0, 0), 0.0);
    }

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
