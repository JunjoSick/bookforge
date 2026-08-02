//! End-to-end conversion orchestration: poppler → parse → reconstruct →
//! EPUB + report.

use std::{
    collections::{HashMap, HashSet, hash_map::DefaultHasher},
    fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
};

use bookforge_core::math::{is_inline_math_operator, is_strong_inline_math_operator};

use crate::{
    Result,
    epub::{
        ChapterSplitOutcome, MAX_CHAPTER_MATCHES, MIN_TEXT_BLOCKS_PER_MATCH,
        write_epub_with_chapter_prefix,
    },
    model::{
        ColumnMode, DocBlock, Fragment, ImageAsset, ImageRegion, LowConfidenceMode, Page, Span,
        normalize_text_key, spans_text,
    },
    ocr::OcrEngine,
    parse::parse_pdf2xml,
    reconstruct::{AnchoredBlock, BlockAnchor, PageStats, reconstruct},
    report::{ConversionReport, LOW_CONFIDENCE_COVERAGE_RATIO, ReportMetrics},
    tools::{ExtractedImage, PageCrop, PopplerTools, crop_png_to_file, scoped_temp_dir},
};

mod detection;
mod rendering;
mod reporting;

use detection::{
    AnchoredFigure, MediaKind, RegionRect, figure_blocks_from_images, media_figure_blocks,
};
#[cfg(test)]
use detection::{
    detect_caption_fragment, detect_media_regions, image_candidate_clusters,
    image_figure_candidates, is_display_equation_fragment, is_prose_like_fragment,
    padded_vector_crop_rect, table_regions_for_page, vector_figure_regions,
};
#[cfg(test)]
use rendering::remove_blocks_in_region;
use rendering::{
    PageCropRenderer, image_asset, insert_figure_blocks, media_asset, ocr_low_confidence_pages,
    preserve_low_confidence_pages, render_figure_crop,
};
use reporting::{baseline_page_char_counts, mark_low_confidence_pages, media_layout_warnings};

const MIN_REGIONLESS_IMAGE_AREA: u32 = 16_384;
const REPEATED_IMAGE_PAGE_THRESHOLD: usize = 3;

#[derive(Debug, Clone)]
pub struct ConvertOptions {
    pub columns: ColumnMode,
    pub low_confidence: LowConfidenceMode,
    /// dc:language for the produced EPUB (source language of the PDF).
    pub language: String,
    /// dc:title; defaults to the input file stem when empty.
    pub title: String,
    /// Case-insensitive literal prefix that starts a new EPUB chapter after
    /// whitespace normalization. `None` preserves the legacy single chapter.
    pub chapter_prefix: Option<String>,
}

impl Default for ConvertOptions {
    fn default() -> Self {
        Self {
            columns: ColumnMode::Auto,
            low_confidence: LowConfidenceMode::Linearize,
            language: "en".to_string(),
            title: String::new(),
            chapter_prefix: None,
        }
    }
}

pub struct ConvertOutcome {
    pub output: PathBuf,
    pub report: ConversionReport,
    pub chapters: usize,
    pub blocks_per_chapter: Vec<usize>,
}

pub fn convert_pdf(
    input: &Path,
    output: &Path,
    options: &ConvertOptions,
) -> Result<ConvertOutcome> {
    convert_pdf_with_ocr(input, output, options, None)
}

pub fn convert_pdf_with_ocr(
    input: &Path,
    output: &Path,
    options: &ConvertOptions,
    ocr: Option<&dyn OcrEngine>,
) -> Result<ConvertOutcome> {
    let tools = PopplerTools::discover()?;
    convert_pdf_with_tools(input, output, options, &tools, ocr)
}

fn convert_pdf_with_tools(
    input: &Path,
    output: &Path,
    options: &ConvertOptions,
    tools: &PopplerTools,
    ocr: Option<&dyn OcrEngine>,
) -> Result<ConvertOutcome> {
    let xml = tools.pdf_to_xml(input)?;
    let pages = parse_pdf2xml(&xml)?;
    let reconstruction = reconstruct(&pages, options.columns);
    let baseline = tools.pdf_to_text(input)?;
    let baseline_chars = baseline.chars().filter(|ch| !ch.is_whitespace()).count();
    let baseline_page_chars = baseline_page_char_counts(&baseline, reconstruction.pages.len());
    let mut page_stats = reconstruction.pages;
    for (stats, chars) in page_stats.iter_mut().zip(baseline_page_chars) {
        stats.baseline_chars = chars;
    }
    let mut low_confidence_pages =
        mark_low_confidence_pages(&mut page_stats, options.low_confidence);

    let media_dir = scoped_temp_dir("bookforge-pdf-media")?;
    let image_dir = scoped_temp_dir("bookforge-pdf-images")?;
    let page_render_dir = scoped_temp_dir("bookforge-pdf-page-renders")?;
    let mut layout_warnings = Vec::new();
    let extracted_images = match tools.extract_images(input, &image_dir) {
        Ok(images) => images,
        Err(err) => {
            layout_warnings.push(format!(
                "image extraction unavailable; continuing text-only for embedded images: {err}"
            ));
            Vec::new()
        }
    };
    let mut crop_renderer = PageCropRenderer::new(input, tools, &page_render_dir);
    let figure_result = figure_blocks_from_images(
        &pages,
        &page_stats,
        &extracted_images,
        &mut crop_renderer,
        &media_dir,
    )?;
    let media_exclusions = figure_result.exclusions.clone();
    let mut figure_blocks = figure_result.figures;
    layout_warnings.extend(figure_result.warnings);
    let media_figures =
        media_figure_blocks(&pages, &media_exclusions, &mut crop_renderer, &media_dir)?;
    let _ = fs::remove_dir_all(&media_dir);
    let _ = fs::remove_dir_all(&image_dir);
    let _ = fs::remove_dir_all(&page_render_dir);
    layout_warnings.extend(media_figures.warnings);
    figure_blocks.extend(media_figures.figures);
    let mut blocks = reconstruction.blocks;
    let media_preserved_chars =
        insert_figure_blocks(&mut blocks, figure_blocks, &mut layout_warnings);
    if let Some(engine) = ocr {
        let ocr_page_dir = scoped_temp_dir("bookforge-pdf-ocr-pages")?;
        let mut render = |page_number| {
            let path = tools.render_page_png(input, page_number, &ocr_page_dir)?;
            Ok(fs::read(path)?)
        };
        let ocr_outcome =
            ocr_low_confidence_pages(engine, &mut render, &mut blocks, &low_confidence_pages);
        let _ = fs::remove_dir_all(&ocr_page_dir);
        for stats in &mut page_stats {
            if ocr_outcome.recovered.contains(&stats.page) {
                stats.low_confidence_action = Some("ocr".to_string());
            }
        }
        low_confidence_pages.retain(|page| !ocr_outcome.recovered.contains(page));
        layout_warnings.extend(ocr_outcome.warnings);
    }
    if options.low_confidence == LowConfidenceMode::Preserve {
        layout_warnings.extend(preserve_low_confidence_pages(
            input,
            &pages,
            tools,
            &mut blocks,
            &low_confidence_pages,
        )?);
    }
    layout_warnings.extend(media_layout_warnings(&blocks));
    let reconstructed_chars: usize = blocks
        .iter()
        .map(|anchored| anchored.block.char_count())
        .sum();
    let figure_count = blocks
        .iter()
        .filter(|anchored| matches!(anchored.block, DocBlock::Figure { .. }))
        .count();
    let output_blocks = blocks
        .iter()
        .map(|anchored| anchored.block.clone())
        .collect::<Vec<_>>();

    let title = if options.title.is_empty() {
        input
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Converted PDF".to_string())
    } else {
        options.title.clone()
    };
    let chapter_outcome = write_epub_with_chapter_prefix(
        &output_blocks,
        &title,
        &options.language,
        output,
        options.chapter_prefix.as_deref(),
    )?;
    let blocks_per_chapter = match chapter_outcome {
        ChapterSplitOutcome::SingleChapter => vec![output_blocks.len()],
        ChapterSplitOutcome::Split { blocks_per_chapter } => blocks_per_chapter,
        ChapterSplitOutcome::Guarded {
            matches,
            text_blocks,
        } => {
            layout_warnings.push(format!(
                "chapter prefix matched {matches} of {text_blocks} text blocks; kept the legacy single chapter because the split guard allows at most {MAX_CHAPTER_MATCHES} matches and requires at least {MIN_TEXT_BLOCKS_PER_MATCH} text blocks per match"
            ));
            vec![output_blocks.len()]
        }
    };

    let report = ConversionReport::build(
        &input.to_string_lossy(),
        &output.to_string_lossy(),
        page_stats,
        ReportMetrics {
            blocks: output_blocks.len(),
            reconstructed_chars,
            media_preserved_chars,
            baseline_chars,
            images: extracted_images.len(),
            figures: figure_count,
            tables: media_figures.counts.tables,
            equations: media_figures.counts.equations,
            layout_warnings,
        },
    );

    Ok(ConvertOutcome {
        output: output.to_path_buf(),
        report,
        chapters: blocks_per_chapter.len(),
        blocks_per_chapter,
    })
}

#[cfg(test)]
mod tests;
