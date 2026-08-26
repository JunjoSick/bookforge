//! End-to-end conversion orchestration: poppler → parse → reconstruct →
//! EPUB + report.

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use bookforge_core::math::{is_inline_math_operator, is_strong_inline_math_operator};

use crate::{
    Result,
    epub::{
        ChapterSplitOutcome, MAX_CHAPTER_MATCHES, MIN_TEXT_BLOCKS_PER_MATCH, publication_timestamp,
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
    tools::{
        ExtractedImage, PDF_RENDER_DPI, PDF_XML_ZOOM_DEN, PDF_XML_ZOOM_NUM, PageCrop, PopplerTools,
        ScopedTempDir, crop_png_to_file,
    },
};

mod detection;
mod rendering;
mod reporting;

use crate::ocr::MAX_OCR_REQUEST_BODY_BYTES;
use detection::{
    AnchoredFigure, MediaKind, RegionRect, figure_blocks_from_images, media_figure_blocks,
    skipped_foreign_caption_warning,
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

/// Lossless raster budget for a single OCR request: at 150 DPI an A4
/// page is ~2.2 MP, so anything near this ceiling is an extreme
/// MediaBox and gets downscaled before pdftoppm allocates it
/// (docs/report.md §4.5 PDF-22).
pub(crate) const MAX_OCR_RENDER_PIXELS: u64 = 32_000_000;

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

    // Scratch directories are RAII-guarded so every error path (`?`) in
    // the figure/media passes still removes them (PDF-3); previously a
    // failing pass leaked all three.
    let media_dir = ScopedTempDir::new("bookforge-pdf-media")?;
    let image_dir = ScopedTempDir::new("bookforge-pdf-images")?;
    let page_render_dir = ScopedTempDir::new("bookforge-pdf-page-renders")?;
    let mut layout_warnings = Vec::new();
    let extracted_images = match tools.extract_images(input, image_dir.path()) {
        Ok(images) => images,
        Err(err) => {
            layout_warnings.push(format!(
                "image extraction unavailable; continuing text-only for embedded images: {err}"
            ));
            Vec::new()
        }
    };
    let mut crop_renderer = PageCropRenderer::new(input, tools, page_render_dir.path());
    let figure_result = figure_blocks_from_images(
        &pages,
        &page_stats,
        &extracted_images,
        &mut crop_renderer,
        media_dir.path(),
    )?;
    let media_exclusions = figure_result.exclusions.clone();
    let mut figure_blocks = figure_result.figures;
    layout_warnings.extend(figure_result.warnings);
    if reconstruction.rotated_dropped_fragments > 0 {
        layout_warnings.push(format!(
            "{} rotated/zero-width text fragment(s) were excluded from the reading flow (vertical labels, margin watermarks or sidebar decorations); review the affected pages if any of it is meaningful",
            reconstruction.rotated_dropped_fragments
        ));
    }
    if let Some(caption_warning) = skipped_foreign_caption_warning(&pages) {
        layout_warnings.push(caption_warning);
    }
    let media_figures = media_figure_blocks(
        &pages,
        &media_exclusions,
        &mut crop_renderer,
        media_dir.path(),
    )?;
    layout_warnings.extend(media_figures.warnings);
    figure_blocks.extend(media_figures.figures);
    drop(media_dir);
    drop(image_dir);
    drop(page_render_dir);
    let mut blocks = reconstruction.blocks;
    let media_preserved_chars =
        insert_figure_blocks(&mut blocks, figure_blocks, &mut layout_warnings);
    if let Some(engine) = ocr {
        let ocr_page_dir = ScopedTempDir::new("bookforge-pdf-ocr-pages")?;
        let mut render = |page_number| {
            let path = render_ocr_page_png(input, &pages, page_number, tools, ocr_page_dir.path())?;
            Ok(fs::read(path)?)
        };
        let ocr_outcome = ocr_low_confidence_pages(
            engine,
            &mut render,
            &mut blocks,
            &low_confidence_pages,
            MAX_OCR_REQUEST_BODY_BYTES,
        );
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
    refresh_page_char_counts(&mut page_stats, &blocks);
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
    let source_id = pdf_source_identifier(input)?;
    let modified = publication_timestamp();
    let chapter_outcome = write_epub_with_chapter_prefix(
        &output_blocks,
        &title,
        &options.language,
        output,
        options.chapter_prefix.as_deref(),
        &source_id,
        &modified,
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

/// Render one page PNG for OCR, downscaling extreme MediaBoxes so that
/// pdftoppm's raster allocation stays within [`MAX_OCR_RENDER_PIXELS`]
/// before any base64 encoding happens (PDF-22).
fn render_ocr_page_png(
    input: &Path,
    pages: &[Page],
    page_number: u32,
    tools: &PopplerTools,
    output_dir: &Path,
) -> Result<PathBuf> {
    let dpi = match pages.iter().find(|page| page.number == page_number) {
        Some(page) => {
            let zoom = PDF_XML_ZOOM_NUM as f64 / PDF_XML_ZOOM_DEN as f64;
            let width_pts = page.width.max(1) as f64 / zoom;
            let height_pts = page.height.max(1) as f64 / zoom;
            max_ocr_render_dpi(width_pts, height_pts, MAX_OCR_RENDER_PIXELS)
        }
        None => PDF_RENDER_DPI,
    };
    Ok(tools.render_page_png_scaled(input, page_number, output_dir, dpi)?)
}

/// The DPI at which `width_pts × height_pts` renders at most
/// `cap_pixels`, clamped into `[8, PDF_RENDER_DPI]`. Extreme MediaBoxes
/// may land far below the floor's quality expectations — the point is a
/// hard allocation bound; such pages are additionally filtered by the
/// request-body cap.
fn max_ocr_render_dpi(width_pts: f64, height_pts: f64, cap_pixels: u64) -> u32 {
    const MIN_DPI: f64 = 8.0;
    let area_pts = width_pts.max(1.0) * height_pts.max(1.0);
    let cap = cap_pixels.max(1) as f64;
    let dpi = (cap * 72.0 * 72.0 / area_pts).sqrt();
    dpi.round().clamp(MIN_DPI, f64::from(PDF_RENDER_DPI)) as u32
}

/// Refresh per-page character counts from the final block list so OCR
/// replacement and image preservation are reflected in the report
/// instead of leaving stale pre-replacement numbers behind.
fn refresh_page_char_counts(page_stats: &mut [PageStats], blocks: &[AnchoredBlock]) {
    for stats in page_stats {
        stats.chars = blocks
            .iter()
            .filter(|anchored| anchored.anchor.page == stats.page)
            .map(|anchored| match &anchored.block {
                // Raster media content is credited via
                // `media_preserved_chars`; it is not translatable text.
                DocBlock::Figure { .. } => 0,
                _ => anchored.block.char_count(),
            })
            .sum();
        stats.lines = blocks
            .iter()
            .filter(|anchored| anchored.anchor.page == stats.page)
            .count();
    }
}

/// Deterministic unique identifier derived from the PDF bytes:
/// length + CRC-32 checksum streamed in fixed-size chunks so no input
/// size is ever fully buffered just for identity purposes (PDF-14).
fn pdf_source_identifier(input: &Path) -> Result<String> {
    use std::io::Read;

    let file = fs::File::open(input)?;
    let len = file.metadata()?.len();
    let mut reader = std::io::BufReader::with_capacity(64 * 1024, file);
    let mut hasher = crc32fast::Hasher::new();
    let mut chunk = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        hasher.update(&chunk[..read]);
    }
    Ok(format!("bookforge-pdf-{len:x}-{:08x}", hasher.finalize()))
}

#[cfg(test)]
mod tests;
