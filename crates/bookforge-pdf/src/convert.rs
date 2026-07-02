//! End-to-end conversion orchestration: poppler → parse → reconstruct →
//! EPUB + report.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    Result,
    epub::write_epub,
    model::{ColumnMode, DocBlock, ImageAsset, ImageRegion, Page, Span},
    parse::parse_pdf2xml,
    reconstruct::{BlockAnchor, reconstruct},
    report::{ConversionReport, ReportMetrics},
    tools::{ExtractedImage, PopplerTools},
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
    let image_dir = scoped_temp_dir("bookforge-pdf-images")?;
    let extracted_images = tools.extract_images(input, &image_dir)?;
    let figure_blocks = figure_blocks_from_images(&pages, &extracted_images)?;
    let mut blocks = reconstruction.blocks;
    let mut block_anchors = reconstruction.block_anchors;
    let figure_count = insert_figure_blocks(&mut blocks, &mut block_anchors, figure_blocks);
    let _ = fs::remove_dir_all(&image_dir);
    let baseline = tools.pdf_to_text(input)?;
    let baseline_chars = baseline.chars().filter(|ch| !ch.is_whitespace()).count();
    let baseline_page_chars = baseline_page_char_counts(&baseline, reconstruction.pages.len());
    let reconstructed_chars: usize = blocks.iter().map(DocBlock::char_count).sum();

    let title = if options.title.is_empty() {
        input
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Converted PDF".to_string())
    } else {
        options.title.clone()
    };
    write_epub(&blocks, &title, &options.language, output)?;

    let mut page_stats = reconstruction.pages;
    for (stats, chars) in page_stats.iter_mut().zip(baseline_page_chars) {
        stats.baseline_chars = chars;
    }

    let report = ConversionReport::build(
        &input.to_string_lossy(),
        &output.to_string_lossy(),
        page_stats,
        ReportMetrics {
            blocks: blocks.len(),
            reconstructed_chars,
            baseline_chars,
            images: extracted_images.len(),
            figures: figure_count,
        },
    );

    Ok(ConvertOutcome {
        output: output.to_path_buf(),
        report,
    })
}

struct AnchoredFigure {
    block: DocBlock,
    anchor: BlockAnchor,
}

fn figure_blocks_from_images(
    pages: &[Page],
    images: &[ExtractedImage],
) -> Result<Vec<AnchoredFigure>> {
    let pages_by_number = pages
        .iter()
        .map(|page| (page.number, page))
        .collect::<HashMap<_, _>>();
    let mut page_image_counts: HashMap<u32, usize> = HashMap::new();
    let mut figures = Vec::new();

    for image in images {
        let page = pages_by_number.get(&image.page).copied();
        let page_index = page_image_counts.entry(image.page).or_default();
        let region = page.and_then(|page| page.images.get(*page_index));
        *page_index += 1;

        let caption = page.and_then(|page| detect_caption(page, region));
        let top = region.map(|region| region.top).unwrap_or(i32::MAX);
        let asset = image_asset(image, region)?;
        figures.push(AnchoredFigure {
            block: DocBlock::Figure {
                image: asset,
                caption,
            },
            anchor: BlockAnchor {
                page: image.page,
                top,
            },
        });
    }

    Ok(figures)
}

fn image_asset(image: &ExtractedImage, region: Option<&ImageRegion>) -> Result<ImageAsset> {
    let bytes = fs::read(&image.path)?;
    let extension = match image.extension.as_str() {
        "jpg" | "jpeg" => "jpg",
        _ => "png",
    };
    let media_type = match extension {
        "jpg" => "image/jpeg",
        _ => "image/png",
    };
    let id = format!("pdf-image-{:04}", image.index + 1);
    Ok(ImageAsset {
        id,
        href: format!("images/pdf-image-{:04}.{extension}", image.index + 1),
        media_type: media_type.to_string(),
        bytes,
        page: image.page,
        top: region.map(|region| region.top),
        left: region.map(|region| region.left),
        width: region
            .map(|region| region.width)
            .or_else(|| image.width.map(|width| width as i32)),
        height: region
            .map(|region| region.height)
            .or_else(|| image.height.map(|height| height as i32)),
    })
}

fn detect_caption(page: &Page, region: Option<&ImageRegion>) -> Option<Vec<Span>> {
    let mut candidates = page
        .fragments
        .iter()
        .filter(|fragment| is_caption_text(&fragment_text(&fragment.spans)))
        .collect::<Vec<_>>();
    candidates.sort_by_key(|fragment| fragment.top);

    if let Some(region) = region {
        let bottom = region.bottom();
        candidates
            .into_iter()
            .filter(|fragment| fragment.top >= bottom.saturating_sub(8))
            .min_by_key(|fragment| fragment.top.saturating_sub(bottom))
            .filter(|fragment| fragment.top.saturating_sub(bottom) <= 160)
            .map(|fragment| fragment.spans.clone())
    } else {
        candidates.first().map(|fragment| fragment.spans.clone())
    }
}

fn is_caption_text(text: &str) -> bool {
    let lower = text.trim_start().to_ascii_lowercase();
    lower.starts_with("figure ")
        || lower.starts_with("figure\u{00a0}")
        || lower.starts_with("fig. ")
        || lower.starts_with("fig.\u{00a0}")
        || lower.starts_with("fig ")
        || lower.starts_with("table ")
        || lower.starts_with("table\u{00a0}")
}

fn insert_figure_blocks(
    blocks: &mut Vec<DocBlock>,
    block_anchors: &mut Vec<BlockAnchor>,
    mut figures: Vec<AnchoredFigure>,
) -> usize {
    figures.sort_by_key(|figure| (figure.anchor.page, figure.anchor.top));
    let count = figures.len();

    for figure in figures {
        if let DocBlock::Figure {
            caption: Some(caption),
            ..
        } = &figure.block
        {
            remove_duplicate_caption_block(
                blocks,
                block_anchors,
                figure.anchor.page,
                &fragment_text(caption),
            );
        }

        let insert_at = block_anchors
            .iter()
            .position(|anchor| {
                anchor.page > figure.anchor.page
                    || anchor.page == figure.anchor.page && anchor.top > figure.anchor.top
            })
            .unwrap_or(blocks.len());
        blocks.insert(insert_at, figure.block);
        block_anchors.insert(insert_at, figure.anchor);
    }

    count
}

fn remove_duplicate_caption_block(
    blocks: &mut Vec<DocBlock>,
    block_anchors: &mut Vec<BlockAnchor>,
    page: u32,
    caption: &str,
) {
    let normalized = normalize_caption(caption);
    let Some(index) = blocks.iter().enumerate().position(|(index, block)| {
        block_anchors
            .get(index)
            .is_some_and(|anchor| anchor.page == page)
            && matches!(block, DocBlock::Paragraph { .. } | DocBlock::Heading { .. })
            && normalize_caption(&block.text()) == normalized
    }) else {
        return;
    };

    blocks.remove(index);
    block_anchors.remove(index);
}

fn normalize_caption(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn fragment_text(spans: &[Span]) -> String {
    spans
        .iter()
        .map(|span| span.text.as_str())
        .collect::<String>()
}

fn scoped_temp_dir(prefix: &str) -> Result<PathBuf> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let path = std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&path)?;
    Ok(path)
}

fn baseline_page_char_counts(text: &str, pages: usize) -> Vec<usize> {
    let mut counts = text
        .split('\x0c')
        .map(|page| page.chars().filter(|ch| !ch.is_whitespace()).count())
        .collect::<Vec<_>>();
    while counts.last() == Some(&0) && counts.len() > pages {
        counts.pop();
    }
    counts.resize(pages, 0);
    counts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn write_executable(path: &Path, script: &str) {
        use std::{fs, os::unix::fs::PermissionsExt};

        fs::write(path, script).expect("tool fixture");
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("tool executable");
    }

    #[cfg(unix)]
    fn fake_pdfimages(path: &Path) {
        write_executable(
            path,
            r#"#!/bin/sh
if [ "$1" = "-list" ]; then
cat <<'LIST'
page   num  type   width height color comp bpc  enc interp  object ID x-ppi y-ppi size ratio
--------------------------------------------------------------------------------------------
   1     0 image     120    80  rgb     3   8  image  no        12  0    72    72  1K  1.0%
LIST
exit 0
fi
for last do :; done
printf 'fake-image' > "$last-000-000.png"
echo "$last-000-000.png"
"#,
        );
    }

    #[cfg(unix)]
    #[test]
    fn convert_pdf_does_not_write_epub_when_baseline_fails() {
        use std::fs;

        let dir = tempfile::tempdir().expect("temp dir");
        let input = dir.path().join("input.pdf");
        let output = dir.path().join("output.epub");
        fs::write(&input, b"dummy pdf").expect("input pdf fixture");

        let pdftohtml = dir.path().join("pdftohtml");
        write_executable(
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
        );

        let pdftotext = dir.path().join("pdftotext");
        write_executable(
            &pdftotext,
            r#"#!/bin/sh
echo baseline failed >&2
exit 9
"#,
        );
        let pdfimages = dir.path().join("pdfimages");
        fake_pdfimages(&pdfimages);

        let tools = PopplerTools {
            pdftohtml,
            pdftotext,
            pdfimages,
        };

        let result = convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), &tools);

        assert!(result.is_err());
        assert!(
            !output.exists(),
            "output EPUB should not be written after baseline failure"
        );
    }

    #[cfg(unix)]
    #[test]
    fn convert_pdf_embeds_extracted_image_with_translatable_caption() {
        use std::{fs, io::Read};
        use zip::ZipArchive;

        let dir = tempfile::tempdir().expect("temp dir");
        let input = dir.path().join("input.pdf");
        let output = dir.path().join("output.epub");
        fs::write(&input, b"dummy pdf").expect("input pdf fixture");

        let pdftohtml = dir.path().join("pdftohtml");
        write_executable(
            &pdftohtml,
            r##"#!/bin/sh
cat <<'XML'
<pdf2xml>
  <page number="1" width="600" height="800">
    <fontspec id="0" size="14" family="Times" color="#000000"/>
    <fontspec id="1" size="12" family="Times" color="#000000"/>
    <text top="80" left="100" width="300" height="16" font="0">Paper Title</text>
    <image top="130" left="120" width="120" height="80" src="paper-1_1.png"/>
    <text top="218" left="120" width="260" height="12" font="1">Figure 1. A test image.</text>
    <text top="280" left="100" width="300" height="12" font="1">Body text after the figure.</text>
  </page>
</pdf2xml>
XML
"##,
        );

        let pdftotext = dir.path().join("pdftotext");
        write_executable(
            &pdftotext,
            r#"#!/bin/sh
printf 'Paper Title\nFigure 1. A test image.\nBody text after the figure.\n'
"#,
        );
        let pdfimages = dir.path().join("pdfimages");
        fake_pdfimages(&pdfimages);

        let tools = PopplerTools {
            pdftohtml,
            pdftotext,
            pdfimages,
        };
        let outcome = convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), &tools)
            .expect("conversion should succeed");

        assert_eq!(outcome.report.images, 1);
        assert_eq!(outcome.report.figures, 1);

        let mut archive =
            ZipArchive::new(fs::File::open(&output).expect("epub opens")).expect("zip opens");
        let mut content = String::new();
        archive
            .by_name("content.xhtml")
            .expect("content exists")
            .read_to_string(&mut content)
            .expect("content reads");
        assert!(content.contains("<figure id=\"pdf-image-0001\">"));
        assert!(content.contains("<figcaption>Figure 1. A test image.</figcaption>"));
        assert_eq!(content.matches("Figure 1. A test image.").count(), 1);

        let mut image = Vec::new();
        archive
            .by_name("images/pdf-image-0001.png")
            .expect("image exists")
            .read_to_end(&mut image)
            .expect("image reads");
        assert_eq!(image, b"fake-image");

        let book = bookforge_epub::read_epub(&output).expect("converted EPUB should be readable");
        assert!(
            book.blocks
                .iter()
                .any(|block| matches!(block.kind, bookforge_core::ir::BlockKind::Caption)),
            "figcaption should remain a normal translatable caption block"
        );
    }
}
