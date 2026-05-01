use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use anyhow::Result;
use bookforge_core::segment::{Segment, SegmentStatus};
use bookforge_llm::{QaSegmentReview, SegmentTranslation};
use bookforge_store::{JobRecord, JobSummary, SegmentRecord};
use serde::Serialize;

use crate::cost::estimate_cost_usd;

#[derive(Debug, Clone)]
pub(crate) struct ReportFiles {
    pub json: PathBuf,
    pub markdown: PathBuf,
}

#[derive(Debug)]
pub(crate) struct ReportInput<'a> {
    pub job: &'a JobRecord,
    pub summary: &'a JobSummary,
    pub segments: &'a [Segment],
    pub segment_records: &'a [SegmentRecord],
    pub translations: &'a [SegmentTranslation],
    pub qa_reviews: &'a [QaSegmentReview],
    pub output: &'a Path,
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
    input_tokens: u64,
    output_tokens: u64,
    estimated_cost: Option<f64>,
    qa_reviewed_segments: usize,
    qa_warnings: Vec<QaWarning>,
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
        input_tokens: input.summary.input_tokens,
        output_tokens: input.summary.output_tokens,
        estimated_cost: estimate_cost_usd(
            &input.job.provider,
            &input.job.model,
            input.summary.input_tokens,
            input.summary.output_tokens,
        ),
        qa_reviewed_segments: input.qa_reviews.len(),
        qa_warnings: qa_warnings(&input),
    };

    if let Some(parent) = files.json.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&files.json, serde_json::to_string_pretty(&report)?)?;
    fs::write(&files.markdown, render_markdown(&report))?;
    Ok(files)
}

fn report_paths(output: &Path) -> ReportFiles {
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
        if translation.status != SegmentStatus::Succeeded {
            continue;
        }
        let Some(source) = source_by_segment.get(translation.segment_id.0.as_str()) else {
            continue;
        };
        let translated = translation.joined_text();
        let source_len = source.chars().count().max(1);
        let translated_len = translated.chars().count();
        if source_len >= 40 {
            let ratio = translated_len as f64 / source_len as f64;
            if !(0.33..=3.0).contains(&ratio)
                && seen.insert((translation.segment_id.0.clone(), "length_ratio"))
            {
                warnings.push(QaWarning {
                    severity: "warning",
                    kind: "length_ratio",
                    segment_id: Some(translation.segment_id.0.clone()),
                    message: format!(
                        "translated length ratio is suspicious: {ratio:.2} ({source_len} source chars, {translated_len} target chars)"
                    ),
                });
            }
        }

        if source_len >= 40
            && source.trim() == translated.trim()
            && seen.insert((translation.segment_id.0.clone(), "untranslated"))
        {
            warnings.push(QaWarning {
                severity: "warning",
                kind: "untranslated",
                segment_id: Some(translation.segment_id.0.clone()),
                message: "translation is identical to the source text".to_string(),
            });
        }

        if let Some(message) = missing_tokens_message("URL", &urls(source), &urls(&translated))
            && seen.insert((translation.segment_id.0.clone(), "url_changed"))
        {
            warnings.push(QaWarning {
                severity: "warning",
                kind: "url_changed",
                segment_id: Some(translation.segment_id.0.clone()),
                message,
            });
        }

        if let Some(message) =
            missing_tokens_message("number", &numbers(source), &numbers(&translated))
            && seen.insert((translation.segment_id.0.clone(), "number_changed"))
        {
            warnings.push(QaWarning {
                severity: "warning",
                kind: "number_changed",
                segment_id: Some(translation.segment_id.0.clone()),
                message,
            });
        }

        if looks_like_model_commentary(&translated)
            && seen.insert((translation.segment_id.0.clone(), "model_commentary"))
        {
            warnings.push(QaWarning {
                severity: "warning",
                kind: "model_commentary",
                segment_id: Some(translation.segment_id.0.clone()),
                message: "translation appears to include model commentary".to_string(),
            });
        }

        if has_repetition(&translated)
            && seen.insert((translation.segment_id.0.clone(), "repetition"))
        {
            warnings.push(QaWarning {
                severity: "warning",
                kind: "repetition",
                segment_id: Some(translation.segment_id.0.clone()),
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
                    message: issue.clone(),
                });
            }
        }
    }

    warnings
}

fn urls(text: &str) -> Vec<String> {
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

fn numbers(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter_map(|token| {
            let value = token.trim_matches(|ch: char| {
                matches!(
                    ch,
                    ',' | ';' | ':' | '.' | '!' | '?' | '(' | ')' | '[' | ']' | '"' | '\''
                )
            });
            let digits = value.chars().filter(|ch| ch.is_ascii_digit()).count();
            (digits >= 2
                && value.chars().all(|ch| {
                    ch.is_ascii_digit()
                        || matches!(ch, '.' | ',' | ':' | '/' | '-' | '+' | '%' | '$')
                }))
            .then(|| value.to_string())
        })
        .collect()
}

fn missing_tokens_message(label: &str, source: &[String], translated: &[String]) -> Option<String> {
    let missing = source
        .iter()
        .filter(|token| !translated.contains(token))
        .cloned()
        .collect::<Vec<_>>();
    (!missing.is_empty()).then(|| format!("missing preserved {label}(s): {}", missing.join(", ")))
}

fn looks_like_model_commentary(text: &str) -> bool {
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
    output.push_str(&format!("- Input tokens: {}\n", report.input_tokens));
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

    output
}
