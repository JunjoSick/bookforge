//! End-to-end conversion orchestration: poppler → parse → reconstruct →
//! EPUB + report.

use std::path::{Path, PathBuf};

use crate::{
    Result,
    epub::write_epub,
    model::{ColumnMode, DocBlock},
    parse::parse_pdf2xml,
    reconstruct::reconstruct,
    report::ConversionReport,
    tools::PopplerTools,
};

#[derive(Debug, Clone)]
pub struct ConvertOptions {
    pub columns: ColumnMode,
    /// dc:language for the produced EPUB (source language of the PDF).
    pub language: String,
    /// dc:title; defaults to the input file stem when empty.
    pub title: String,
}

impl Default for ConvertOptions {
    fn default() -> Self {
        Self {
            columns: ColumnMode::Auto,
            language: "en".to_string(),
            title: String::new(),
        }
    }
}

pub struct ConvertOutcome {
    pub output: PathBuf,
    pub report: ConversionReport,
}

pub fn convert_pdf(
    input: &Path,
    output: &Path,
    options: &ConvertOptions,
) -> Result<ConvertOutcome> {
    let tools = PopplerTools::discover()?;

    let xml = tools.pdf_to_xml(input)?;
    let pages = parse_pdf2xml(&xml)?;
    let reconstruction = reconstruct(&pages, options.columns);

    let title = if options.title.is_empty() {
        input
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Converted PDF".to_string())
    } else {
        options.title.clone()
    };
    write_epub(&reconstruction.blocks, &title, &options.language, output)?;

    let baseline = tools.pdf_to_text(input)?;
    let baseline_chars = baseline.chars().filter(|ch| !ch.is_whitespace()).count();
    let reconstructed_chars: usize = reconstruction.blocks.iter().map(DocBlock::char_count).sum();

    let report = ConversionReport::build(
        &input.to_string_lossy(),
        &output.to_string_lossy(),
        reconstruction.pages,
        reconstruction.blocks.len(),
        reconstructed_chars,
        baseline_chars,
    );

    Ok(ConvertOutcome {
        output: output.to_path_buf(),
        report,
    })
}
