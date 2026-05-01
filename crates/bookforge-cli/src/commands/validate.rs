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

    Ok(())
}
