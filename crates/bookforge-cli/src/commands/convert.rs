use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use bookforge_pdf::{ColumnMode, ConvertOptions, LowConfidenceMode, convert_pdf};
use clap::Args;

#[derive(Debug, Args)]
pub struct ConvertArgs {
    /// Input PDF.
    pub input: PathBuf,

    /// Output EPUB path. Defaults to the input with an .epub extension.
    #[arg(long)]
    pub out: Option<PathBuf>,

    /// Column handling: detect per page, or force single/two-column.
    #[arg(long, value_enum, default_value_t = ColumnsArg::Auto)]
    pub columns: ColumnsArg,

    /// Source language recorded as the EPUB dc:language.
    #[arg(long, default_value = "en")]
    pub language: String,

    /// Title for the produced EPUB; defaults to the input file name.
    #[arg(long)]
    pub title: Option<String>,

    /// Low-confidence page handling: preserve as page image or keep best-effort text.
    #[arg(long, value_enum, default_value_t = LowConfidenceArg::Linearize)]
    pub low_confidence: LowConfidenceArg,

    /// Where to write the JSON conversion report. Defaults to
    /// `<out>.convert.json`.
    #[arg(long)]
    pub report: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ColumnsArg {
    Auto,
    #[value(name = "1")]
    Single,
    #[value(name = "2")]
    Two,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum LowConfidenceArg {
    Preserve,
    Linearize,
}

impl From<ColumnsArg> for ColumnMode {
    fn from(value: ColumnsArg) -> Self {
        match value {
            ColumnsArg::Auto => ColumnMode::Auto,
            ColumnsArg::Single => ColumnMode::Single,
            ColumnsArg::Two => ColumnMode::Two,
        }
    }
}

impl From<LowConfidenceArg> for LowConfidenceMode {
    fn from(value: LowConfidenceArg) -> Self {
        match value {
            LowConfidenceArg::Preserve => LowConfidenceMode::Preserve,
            LowConfidenceArg::Linearize => LowConfidenceMode::Linearize,
        }
    }
}

pub async fn run(args: ConvertArgs) -> Result<()> {
    if !args.input.exists() {
        bail!("input PDF does not exist: {}", args.input.display());
    }
    let output = args
        .out
        .clone()
        .unwrap_or_else(|| args.input.with_extension("epub"));
    let report_path = args
        .report
        .clone()
        .unwrap_or_else(|| output.with_extension("convert.json"));

    let options = ConvertOptions {
        columns: args.columns.into(),
        low_confidence: args.low_confidence.into(),
        language: args.language.clone(),
        title: args.title.clone().unwrap_or_default(),
    };

    let outcome = convert_pdf(&args.input, &output, &options)
        .with_context(|| format!("converting {}", args.input.display()))?;

    let json = serde_json::to_string_pretty(&outcome.report)?;
    std::fs::write(&report_path, json)
        .with_context(|| format!("writing report {}", report_path.display()))?;

    println!("Input: {}", args.input.display());
    println!("Output: {}", outcome.output.display());
    print!("{}", outcome.report.summary());
    println!("Report: {}", report_path.display());
    println!();
    println!("Check coverage before translating:");
    println!("  bookforge inspect \"{}\"", outcome.output.display());
    println!(
        "  bookforge translate \"{}\" --target <language> ...",
        outcome.output.display()
    );

    Ok(())
}
