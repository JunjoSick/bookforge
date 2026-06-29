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
    convert_pdf_with_tools(input, output, options, &tools)
}

fn convert_pdf_with_tools(
    input: &Path,
    output: &Path,
    options: &ConvertOptions,
    tools: &PopplerTools,
) -> Result<ConvertOutcome> {
    let xml = tools.pdf_to_xml(input)?;
    let pages = parse_pdf2xml(&xml)?;
    let reconstruction = reconstruct(&pages, options.columns);
    let baseline = tools.pdf_to_text(input)?;
    let baseline_chars = baseline.chars().filter(|ch| !ch.is_whitespace()).count();
    let reconstructed_chars: usize = reconstruction.blocks.iter().map(DocBlock::char_count).sum();

    let title = if options.title.is_empty() {
        input
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Converted PDF".to_string())
    } else {
        options.title.clone()
    };
    write_epub(&reconstruction.blocks, &title, &options.language, output)?;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn convert_pdf_does_not_write_epub_when_baseline_fails() {
        use std::{fs, os::unix::fs::PermissionsExt};

        let dir = tempfile::tempdir().expect("temp dir");
        let input = dir.path().join("input.pdf");
        let output = dir.path().join("output.epub");
        fs::write(&input, b"dummy pdf").expect("input pdf fixture");

        let pdftohtml = dir.path().join("pdftohtml");
        fs::write(
            &pdftohtml,
            r##"#!/bin/sh
cat <<'XML'
<pdf2xml>
  <page number="1" width="600" height="800">
    <fontspec id="0" size="12" family="Times" color="#000000"/>
    <text top="100" left="100" width="80" height="12" font="0">Hello PDF</text>
  </page>
</pdf2xml>
XML
"##,
        )
        .expect("pdftohtml fixture");
        fs::set_permissions(&pdftohtml, fs::Permissions::from_mode(0o755))
            .expect("pdftohtml executable");

        let pdftotext = dir.path().join("pdftotext");
        fs::write(
            &pdftotext,
            r#"#!/bin/sh
echo baseline failed >&2
exit 9
"#,
        )
        .expect("pdftotext fixture");
        fs::set_permissions(&pdftotext, fs::Permissions::from_mode(0o755))
            .expect("pdftotext executable");

        let tools = PopplerTools {
            pdftohtml,
            pdftotext,
        };

        let result = convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), &tools);

        assert!(result.is_err());
        assert!(
            !output.exists(),
            "output EPUB should not be written after baseline failure"
        );
    }
}
