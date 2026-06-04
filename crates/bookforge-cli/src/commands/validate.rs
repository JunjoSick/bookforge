use anyhow::Result;
use bookforge_core::config::SegmentationConfig;
use bookforge_core::segment::build_segments;
use bookforge_epub::{inspect_epub, read_epub};
use clap::Args;
use serde_json::Value;
use std::{fs, path::PathBuf};

#[derive(Debug, Args)]
pub struct ValidateArgs {
    pub input: PathBuf,

    #[arg(long)]
    pub report: Option<PathBuf>,
}

pub async fn run(args: ValidateArgs) -> Result<()> {
    println!("Input: {}", args.input.display());

    let inspection = inspect_epub(&args.input)?;
    let book = read_epub(&args.input)?;
    let segments = build_segments(&book, &SegmentationConfig::default())?;
    let token_estimate = segments
        .iter()
        .map(|segment| segment.source.token_estimate)
        .sum::<usize>();

    println!("Package: {}", inspection.package_path);
    println!("Spine count: {}", inspection.spine_count);
    println!("XHTML spine count: {}", inspection.xhtml_spine_count);
    println!("Section count: {}", book.sections.len());
    println!("Block count: {}", book.blocks.len());
    println!("Segment count: {}", segments.len());
    println!("Estimated token count: {token_estimate}");

    if let Some(report_path) = args.report.or_else(|| default_report_path(&args.input)) {
        validate_report(&report_path)?;
    }

    println!("Validation: ok");
    Ok(())
}

fn default_report_path(input: &std::path::Path) -> Option<PathBuf> {
    let stem = input.file_stem()?.to_str()?;
    Some(input.with_file_name(format!("{stem}.report.json")))
}

fn validate_report(path: &PathBuf) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    let parsed = serde_json::from_str::<Value>(&fs::read_to_string(path)?)?;
    let warnings = parsed
        .get("qa_warnings")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let failed = parsed
        .get("failed_segments")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let needs_review = parsed
        .get("needs_review_segments")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let retry_pending = parsed
        .get("retry_pending_segments")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    println!("Report: {}", path.display());
    println!("QA warnings: {warnings}");
    println!("Failed segments: {failed}");
    println!("Needs review segments: {needs_review}");
    println!("Retry pending segments: {retry_pending}");

    if failed > 0 {
        anyhow::bail!("report contains failed segments");
    }
    if needs_review > 0 {
        anyhow::bail!("report contains segments that need review");
    }
    if retry_pending > 0 {
        anyhow::bail!("report contains retry-pending segments");
    }

    let has_error = parsed
        .get("qa_warnings")
        .and_then(Value::as_array)
        .map(|warnings| {
            warnings.iter().any(|w| {
                w.get("severity")
                    .and_then(Value::as_str)
                    .map(|severity| severity == "error")
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);
    if has_error {
        anyhow::bail!("report contains QA warning(s) with severity 'error'");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_report_rejects_error_severity_warning() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let report = temp.path().join("report.json");
        fs::write(
            &report,
            serde_json::json!({
                "qa_warnings": [{
                    "severity": "error",
                    "kind": "qa_review",
                    "segment_id": "seg_a",
                    "message": "bad translation"
                }],
                "failed_segments": 0,
                "needs_review_segments": 0,
                "retry_pending_segments": 0
            })
            .to_string(),
        )
        .expect("report should write");

        let err = validate_report(&report).expect_err("error severity should fail");

        assert!(
            err.to_string()
                .contains("report contains QA warning(s) with severity 'error'")
        );
    }

    #[test]
    fn validate_report_allows_non_error_warnings() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let report = temp.path().join("report.json");
        fs::write(
            &report,
            serde_json::json!({
                "qa_warnings": [{
                    "severity": "warning",
                    "kind": "untranslated",
                    "segment_id": "seg_a",
                    "message": "same title"
                }],
                "failed_segments": 0,
                "needs_review_segments": 0,
                "retry_pending_segments": 0
            })
            .to_string(),
        )
        .expect("report should write");

        validate_report(&report).expect("warning-only report should pass");
    }
}
