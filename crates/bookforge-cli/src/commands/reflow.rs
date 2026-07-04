use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use bookforge_epub::{ReflowOptions, ReflowReport, reflow_epub};
use clap::Args;

#[derive(Debug, Args)]
pub struct ReflowArgs {
    /// Input source EPUB.
    pub input: PathBuf,

    /// Output repaired EPUB path.
    #[arg(long)]
    pub output: PathBuf,

    /// JSON reflow report path. Defaults to `<output>.reflow-report.json`.
    #[arg(long)]
    pub report: Option<PathBuf>,

    /// Write the report and summary without producing an output EPUB.
    #[arg(long)]
    pub dry_run: bool,
}

pub async fn run(args: ReflowArgs) -> Result<()> {
    if !args.input.exists() {
        bail!("input EPUB does not exist: {}", args.input.display());
    }

    let report_path = args
        .report
        .clone()
        .unwrap_or_else(|| default_report_path(&args.output));
    let outcome = reflow_epub(
        &args.input,
        &args.output,
        &ReflowOptions {
            dry_run: args.dry_run,
        },
    )
    .with_context(|| format!("reflowing {}", args.input.display()))?;

    write_report(&report_path, &outcome.report)?;

    println!("Input: {}", args.input.display());
    println!("Output: {}", args.output.display());
    println!("Files checked: {}", outcome.report.totals.files_checked);
    println!("Files touched: {}", outcome.report.totals.files_touched);
    println!("Merges: {}", outcome.report.totals.merge_count);
    println!(
        "Paragraphs: {} -> {}",
        outcome.report.totals.paragraphs_before, outcome.report.totals.paragraphs_after
    );
    println!("Report: {}", report_path.display());
    if args.dry_run {
        println!("Dry run: no EPUB written");
    }

    Ok(())
}

fn write_report(path: &Path, report: &ReflowReport) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating report directory {}", parent.display()))?;
    }
    fs::write(path, serde_json::to_string_pretty(report)?)
        .with_context(|| format!("writing report {}", path.display()))?;
    Ok(())
}

pub(crate) fn default_report_path(output: &Path) -> PathBuf {
    let stem = output
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("book");
    output.with_file_name(format!("{stem}.reflow-report.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_report_path_uses_output_stem() {
        assert_eq!(
            default_report_path(Path::new("out/repaired.epub")),
            PathBuf::from("out/repaired.reflow-report.json")
        );
    }
}
