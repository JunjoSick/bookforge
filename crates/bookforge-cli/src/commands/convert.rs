use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use bookforge_pdf::{
    ColumnMode, ConvertOptions, HttpOcrClient, LowConfidenceMode, OcrConfig, OcrDialect, OcrEngine,
    convert_pdf_with_ocr,
};
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

    /// Start chapters at text blocks beginning with this case-insensitive
    /// literal prefix after whitespace normalization. Omit for one chapter.
    #[arg(long)]
    pub chapter_prefix: Option<String>,

    /// Low-confidence page handling: preserve as page image or keep best-effort text.
    #[arg(long, value_enum, default_value_t = LowConfidenceArg::Linearize)]
    pub low_confidence: LowConfidenceArg,

    /// Where to write the JSON conversion report. Defaults to
    /// `<out>.convert.json`.
    #[arg(long)]
    pub report: Option<PathBuf>,

    /// OpenAI-compatible base URL used to OCR low-confidence pages.
    #[arg(long)]
    pub ocr_endpoint: Option<String>,

    /// OCR server request dialect.
    #[arg(long, value_enum, default_value_t = OcrDialectArg::Openai)]
    pub ocr_dialect: OcrDialectArg,

    /// OCR model name.
    #[arg(long)]
    pub ocr_model: Option<String>,

    /// Environment variable containing the OCR API key.
    #[arg(long)]
    pub ocr_api_key_env: Option<String>,

    /// Prompt sent with each rendered page.
    #[arg(long)]
    pub ocr_prompt: Option<String>,

    /// Unlimited-OCR image processing mode.
    #[arg(long, value_enum, default_value_t = OcrImageModeArg::Gundam)]
    pub ocr_image_mode: OcrImageModeArg,

    /// File containing a serialized Unlimited-OCR custom logit processor.
    #[arg(long)]
    pub ocr_logit_processor: Option<PathBuf>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum OcrDialectArg {
    Openai,
    #[value(name = "unlimited-ocr")]
    UnlimitedOcr,
}

impl OcrDialectArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Openai => "openai",
            Self::UnlimitedOcr => "unlimited-ocr",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum OcrImageModeArg {
    Gundam,
    Base,
}

impl OcrImageModeArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Gundam => "gundam",
            Self::Base => "base",
        }
    }
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
        chapter_prefix: args.chapter_prefix.clone(),
    };

    let mut ocr_config = args.ocr_endpoint.as_ref().map(|endpoint| {
        let mut config = OcrConfig::new(endpoint);
        config.dialect = match args.ocr_dialect {
            OcrDialectArg::Openai => OcrDialect::OpenAiCompatible,
            OcrDialectArg::UnlimitedOcr => OcrDialect::UnlimitedOcr,
        };
        if let Some(model) = &args.ocr_model {
            config.model.clone_from(model);
        }
        if let Some(api_key_env) = &args.ocr_api_key_env {
            config.api_key_env.clone_from(api_key_env);
        }
        if let Some(prompt) = &args.ocr_prompt {
            config.prompt.clone_from(prompt);
        }
        config.image_mode = args.ocr_image_mode.as_str().to_string();
        config
    });
    if let Some(path) = &args.ocr_logit_processor {
        let processor = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("reading OCR logit processor {}", path.display()))?;
        let config = ocr_config
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("--ocr-logit-processor requires --ocr-endpoint"))?;
        config.logit_processor = Some(processor.trim().to_string());
    }
    let ocr_summary = ocr_config.as_ref().map(|config| {
        (
            config.base_url.clone(),
            config.model.clone(),
            args.ocr_dialect.as_str(),
        )
    });

    let input = args.input.clone();
    let conversion_input = input.clone();
    let conversion_output = output.clone();
    // The conversion (and optional OCR of low-confidence pages) can take
    // minutes; previously everything printed only after completion, leaving
    // a silent start. Announce the work up front.
    println!("Converting {} → {} ...", input.display(), output.display());
    let outcome = tokio::task::spawn_blocking(move || -> Result<_> {
        let ocr_client = ocr_config
            .map(HttpOcrClient::new)
            .transpose()
            .context("initializing OCR client")?;
        let engine = ocr_client.as_ref().map(|client| client as &dyn OcrEngine);
        convert_pdf_with_ocr(&conversion_input, &conversion_output, &options, engine)
            .with_context(|| format!("converting {}", conversion_input.display()))
    })
    .await
    .context("PDF conversion worker failed")??;

    let json = serde_json::to_string_pretty(&outcome.report)?;
    std::fs::write(&report_path, json)
        .with_context(|| format!("writing report {}", report_path.display()))?;

    println!("Input: {}", input.display());
    println!("Output: {}", outcome.output.display());
    if args.chapter_prefix.is_some() {
        println!("Chapters: {}", outcome.chapters);
        println!(
            "Blocks per chapter: {}",
            outcome
                .blocks_per_chapter
                .iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if let Some((endpoint, model, dialect)) = ocr_summary {
        println!("OCR: {endpoint} ({model}, {dialect})");
    }
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
