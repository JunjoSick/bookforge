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
    epub::write_epub,
    model::{
        ColumnMode, DocBlock, Fragment, ImageAsset, ImageRegion, LowConfidenceMode, Page, Span,
        normalize_text_key, spans_text,
    },
    parse::parse_pdf2xml,
    reconstruct::{AnchoredBlock, BlockAnchor, PageStats, reconstruct},
    report::{ConversionReport, LOW_CONFIDENCE_COVERAGE_RATIO, ReportMetrics},
    tools::{ExtractedImage, PageCrop, PopplerTools, crop_png_to_file, scoped_temp_dir},
};

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
}

impl Default for ConvertOptions {
    fn default() -> Self {
        Self {
            columns: ColumnMode::Auto,
            low_confidence: LowConfidenceMode::Linearize,
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
    let baseline_page_chars = baseline_page_char_counts(&baseline, reconstruction.pages.len());
    let mut page_stats = reconstruction.pages;
    for (stats, chars) in page_stats.iter_mut().zip(baseline_page_chars) {
        stats.baseline_chars = chars;
    }
    let low_confidence_pages = mark_low_confidence_pages(&mut page_stats, options.low_confidence);

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
    write_epub(&output_blocks, &title, &options.language, output)?;

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
    })
}

fn mark_low_confidence_pages(page_stats: &mut [PageStats], mode: LowConfidenceMode) -> Vec<u32> {
    let action = low_confidence_action(mode);
    let mut pages = Vec::new();
    for stats in page_stats {
        if is_low_confidence_page(stats) {
            stats.low_confidence = true;
            stats.low_confidence_action = Some(action.to_string());
            pages.push(stats.page);
        }
    }
    pages
}

fn is_low_confidence_page(stats: &PageStats) -> bool {
    stats.baseline_chars > 0
        && (stats.chars as f64 / stats.baseline_chars as f64) < LOW_CONFIDENCE_COVERAGE_RATIO
}

fn low_confidence_action(mode: LowConfidenceMode) -> &'static str {
    match mode {
        LowConfidenceMode::Preserve => "preserve",
        LowConfidenceMode::Linearize => "linearize",
    }
}

fn preserve_low_confidence_pages(
    input: &Path,
    pages: &[Page],
    tools: &PopplerTools,
    blocks: &mut Vec<AnchoredBlock>,
    low_confidence_pages: &[u32],
) -> Result<Vec<String>> {
    if low_confidence_pages.is_empty() {
        return Ok(Vec::new());
    }

    let page_dir = scoped_temp_dir("bookforge-pdf-pages")?;
    let mut warnings = Vec::new();
    let result = (|| {
        for page_number in low_confidence_pages {
            let rendered = match tools.render_page_png(input, *page_number, &page_dir) {
                Ok(rendered) => rendered,
                Err(err) => {
                    warnings.push(format!(
                        "page {page_number}: low-confidence page image preservation skipped because raster rendering failed: {err}"
                    ));
                    continue;
                }
            };
            let source_page = pages.iter().find(|page| page.number == *page_number);
            let asset = page_image_asset(*page_number, source_page, &rendered)?;
            replace_page_with_preserved_image(blocks, *page_number, asset);
        }
        Ok(warnings)
    })();
    let _ = fs::remove_dir_all(&page_dir);
    result
}

fn page_image_asset(page_number: u32, page: Option<&Page>, path: &Path) -> Result<ImageAsset> {
    let bytes = fs::read(path)?;
    Ok(ImageAsset {
        id: format!("pdf-page-{page_number:04}"),
        href: format!("images/pdf-page-{page_number:04}.png"),
        media_type: "image/png".to_string(),
        bytes,
        page: page_number,
        top: Some(0),
        left: Some(0),
        width: page.map(|page| page.width),
        height: page.map(|page| page.height),
    })
}

fn replace_page_with_preserved_image(
    blocks: &mut Vec<AnchoredBlock>,
    page_number: u32,
    image: ImageAsset,
) {
    let old_blocks = std::mem::take(blocks);
    let mut insert_at = None;

    for anchored in old_blocks {
        if anchored.anchor.page == page_number {
            insert_at.get_or_insert(blocks.len());
            continue;
        }
        if anchored.anchor.page > page_number {
            insert_at.get_or_insert(blocks.len());
        }
        blocks.push(anchored);
    }

    let insert_at = insert_at.unwrap_or(blocks.len());
    let width = image.width.unwrap_or(1);
    blocks.insert(
        insert_at,
        AnchoredBlock {
            block: DocBlock::Figure {
                image,
                caption: None,
            },
            anchor: BlockAnchor {
                page: page_number,
                top: 0,
                left: 0,
                width,
            },
        },
    );
}

fn media_layout_warnings(blocks: &[AnchoredBlock]) -> Vec<String> {
    let mut warnings = Vec::new();
    for (index, anchored) in blocks.iter().enumerate().skip(1) {
        if !matches!(
            blocks.get(index - 1).map(|anchored| &anchored.block),
            Some(DocBlock::Figure { .. })
        ) || !starts_with_lowercase_or_suffix(&anchored.block)
        {
            continue;
        }
        warnings.push(format!(
            "page {}: lowercase paragraph continuation follows media block near y={}; review paragraph join",
            anchored.anchor.page, anchored.anchor.top
        ));
    }
    warnings
}

fn starts_with_lowercase_or_suffix(block: &DocBlock) -> bool {
    let DocBlock::Paragraph { spans } = block else {
        return false;
    };
    let text = spans_text(spans);
    let trimmed = text.trim_start();
    trimmed.chars().next().is_some_and(|ch| ch.is_lowercase())
        || trimmed.starts_with(',')
        || trimmed.starts_with(';')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaKind {
    Table,
    Equation,
}

impl MediaKind {
    fn id_prefix(self) -> &'static str {
        match self {
            MediaKind::Table => "pdf-table",
            MediaKind::Equation => "pdf-equation",
        }
    }

    fn crop_padding(self) -> i32 {
        match self {
            MediaKind::Table => 8,
            MediaKind::Equation => 10,
        }
    }

    fn label(self) -> &'static str {
        match self {
            MediaKind::Table => "table",
            MediaKind::Equation => "equation",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RegionRect {
    page: u32,
    top: i32,
    left: i32,
    width: i32,
    height: i32,
}

impl RegionRect {
    fn right(self) -> i32 {
        self.left + self.width
    }

    fn bottom(self) -> i32 {
        self.top + self.height
    }

    fn padded(self, page: &Page, padding: i32) -> Self {
        let left = self.left.saturating_sub(padding).max(0);
        let top = self.top.saturating_sub(padding).max(0);
        let right = (self.right() + padding).min(page.width).max(left + 1);
        let bottom = (self.bottom() + padding).min(page.height).max(top + 1);
        Self {
            page: self.page,
            top,
            left,
            width: right - left,
            height: bottom - top,
        }
    }

    fn overlaps_fragment(self, fragment: &crate::model::Fragment) -> bool {
        fragment.top < self.bottom()
            && fragment.top + fragment.height > self.top
            && fragment.left < self.right()
            && fragment.right() > self.left
    }

    fn area(self) -> i64 {
        i64::from(self.width.max(0)) * i64::from(self.height.max(0))
    }

    fn overlap_area(self, other: Self) -> i64 {
        if self.page != other.page {
            return 0;
        }
        let left = self.left.max(other.left);
        let right = self.right().min(other.right());
        let top = self.top.max(other.top);
        let bottom = self.bottom().min(other.bottom());
        i64::from((right - left).max(0)) * i64::from((bottom - top).max(0))
    }

    fn vertically_overlaps(self, other: Self) -> bool {
        self.page == other.page && self.top < other.bottom() && other.top < self.bottom()
    }

    fn touches_within(self, other: Self, gap: i32) -> bool {
        self.page == other.page
            && self.left <= other.right().saturating_add(gap)
            && other.left <= self.right().saturating_add(gap)
            && self.top <= other.bottom().saturating_add(gap)
            && other.top <= self.bottom().saturating_add(gap)
    }
}

fn region_rect_from_image_region(page: u32, region: &ImageRegion) -> RegionRect {
    RegionRect {
        page,
        top: region.top,
        left: region.left,
        width: region.width,
        height: region.height,
    }
}

#[derive(Debug, Clone)]
struct MediaRegion {
    kind: MediaKind,
    rect: RegionRect,
    caption: Option<Vec<Span>>,
}

#[derive(Debug, Clone, Copy, Default)]
struct MediaCounts {
    tables: usize,
    equations: usize,
}

struct MediaFigures {
    figures: Vec<AnchoredFigure>,
    counts: MediaCounts,
    warnings: Vec<String>,
}

struct AnchoredFigure {
    block: DocBlock,
    anchor: BlockAnchor,
    text_region: Option<RegionRect>,
}

struct FigureBlocks {
    figures: Vec<AnchoredFigure>,
    warnings: Vec<String>,
    exclusions: Vec<RegionRect>,
}

#[derive(Debug, Clone, Copy)]
struct ImageFigureCandidate<'a> {
    image: &'a ExtractedImage,
    region: Option<&'a ImageRegion>,
    caption: Option<&'a Fragment>,
}

#[derive(Debug, Clone, Copy)]
struct FigureCropRegion<'a> {
    rect: RegionRect,
    caption: &'a Fragment,
}

struct PageCropRenderer<'a> {
    input: &'a Path,
    tools: &'a PopplerTools,
    page_dir: &'a Path,
    rendered_pages: HashMap<u32, PathBuf>,
}

impl<'a> PageCropRenderer<'a> {
    fn new(input: &'a Path, tools: &'a PopplerTools, page_dir: &'a Path) -> Self {
        Self {
            input,
            tools,
            page_dir,
            rendered_pages: HashMap::new(),
        }
    }

    fn render_page(&mut self, page: u32) -> Result<PathBuf> {
        if let Some(path) = self.rendered_pages.get(&page) {
            return Ok(path.clone());
        }
        let path = self
            .tools
            .render_page_png(self.input, page, self.page_dir)?;
        self.rendered_pages.insert(page, path.clone());
        Ok(path)
    }

    fn render_crop(&mut self, crop: PageCrop, output_dir: &Path, name: &str) -> Result<PathBuf> {
        fs::create_dir_all(output_dir)?;
        let full_page = self.render_page(crop.page)?;
        let output = output_dir.join(format!("{name}.png"));
        crop_png_to_file(&full_page, crop.to_render_pixels(), &output)?;
        Ok(output)
    }
}

fn media_figure_blocks(
    pages: &[Page],
    figure_exclusions: &[RegionRect],
    crop_renderer: &mut PageCropRenderer<'_>,
    output_dir: &Path,
) -> Result<MediaFigures> {
    let regions = detect_media_regions(pages, figure_exclusions);
    let mut figures = Vec::new();
    let mut counts = MediaCounts::default();
    let mut warnings = Vec::new();

    for region in regions {
        let Some(page) = pages.iter().find(|page| page.number == region.rect.page) else {
            continue;
        };
        let crop_rect = region.rect.padded(page, region.kind.crop_padding());
        let index = match region.kind {
            MediaKind::Table => counts.tables + 1,
            MediaKind::Equation => counts.equations + 1,
        };
        let name = format!("{}-{index:04}", region.kind.id_prefix());
        let rendered = match crop_renderer.render_crop(
            PageCrop {
                page: crop_rect.page,
                left: crop_rect.left,
                top: crop_rect.top,
                width: crop_rect.width,
                height: crop_rect.height,
            },
            output_dir,
            &name,
        ) {
            Ok(rendered) => rendered,
            Err(err) => {
                warnings.push(format!(
                    "page {}: skipped {} crop near y={} because raster rendering failed: {err}",
                    region.rect.page,
                    region.kind.label(),
                    region.rect.top
                ));
                continue;
            }
        };
        match region.kind {
            MediaKind::Table => counts.tables += 1,
            MediaKind::Equation => counts.equations += 1,
        }
        let asset = media_asset(region.kind, index, crop_rect, &rendered)?;
        figures.push(AnchoredFigure {
            block: DocBlock::Figure {
                image: asset,
                caption: region.caption,
            },
            anchor: BlockAnchor {
                page: crop_rect.page,
                top: crop_rect.top,
                left: crop_rect.left,
                width: crop_rect.width,
            },
            text_region: Some(region.rect),
        });
    }

    Ok(MediaFigures {
        figures,
        counts,
        warnings,
    })
}

fn detect_media_regions(pages: &[Page], figure_exclusions: &[RegionRect]) -> Vec<MediaRegion> {
    let mut regions = Vec::new();
    for page in pages {
        let mut excluded = media_exclusion_regions_for_page(page, figure_exclusions);
        let mut page_regions = table_regions_for_page(page, &excluded);
        excluded.extend(
            page_regions
                .iter()
                .map(|region| region.rect)
                .collect::<Vec<_>>(),
        );
        page_regions.extend(equation_regions_for_page(page, &excluded));
        regions.extend(page_regions);
    }
    regions.sort_by_key(|region| {
        (
            region.rect.page,
            region.rect.top,
            match region.kind {
                MediaKind::Table => 0,
                MediaKind::Equation => 1,
            },
        )
    });
    regions
}

fn media_exclusion_regions_for_page(
    page: &Page,
    figure_exclusions: &[RegionRect],
) -> Vec<RegionRect> {
    let mut excluded = page
        .images
        .iter()
        .map(|region| region_rect_from_image_region(page.number, region))
        .collect::<Vec<_>>();
    excluded.extend(
        figure_exclusions
            .iter()
            .filter(|region| region.page == page.number)
            .copied(),
    );
    excluded
}

fn media_asset(kind: MediaKind, index: usize, rect: RegionRect, path: &Path) -> Result<ImageAsset> {
    let bytes = fs::read(path)?;
    let prefix = kind.id_prefix();
    Ok(ImageAsset {
        id: format!("{prefix}-{index:04}"),
        href: format!("images/{prefix}-{index:04}.png"),
        media_type: "image/png".to_string(),
        bytes,
        page: rect.page,
        top: Some(rect.top),
        left: Some(rect.left),
        width: Some(rect.width),
        height: Some(rect.height),
    })
}

#[derive(Debug, Clone)]
struct FragmentRow<'a> {
    fragments: Vec<&'a Fragment>,
    top: i32,
    left: i32,
    right: i32,
    bottom: i32,
}

impl<'a> FragmentRow<'a> {
    fn new(fragment: &'a Fragment) -> Self {
        Self {
            fragments: vec![fragment],
            top: fragment.top,
            left: fragment.left,
            right: fragment.right(),
            bottom: fragment.top + fragment.height,
        }
    }

    fn push(&mut self, fragment: &'a Fragment) {
        self.top = self.top.min(fragment.top);
        self.left = self.left.min(fragment.left);
        self.right = self.right.max(fragment.right());
        self.bottom = self.bottom.max(fragment.top + fragment.height);
        self.fragments.push(fragment);
        self.fragments.sort_by_key(|fragment| fragment.left);
    }

    fn height(&self) -> i32 {
        self.bottom - self.top
    }
}

fn fragment_rows(page: &Page) -> Vec<FragmentRow<'_>> {
    let mut fragments = page
        .fragments
        .iter()
        .filter(|fragment| fragment.width > 0 && !spans_text(&fragment.spans).trim().is_empty())
        .collect::<Vec<_>>();
    fragments.sort_by_key(|fragment| (fragment.top, fragment.left));

    let mut rows: Vec<FragmentRow<'_>> = Vec::new();
    for fragment in fragments {
        let same_row = rows.last().is_some_and(|row| {
            let tolerance = (row.height().max(fragment.height) / 2).max(3);
            (fragment.top - row.top).abs() <= tolerance
        });
        if same_row {
            let row = rows.last_mut().expect("checked above");
            row.push(fragment);
        } else {
            rows.push(FragmentRow::new(fragment));
        }
    }
    rows
}

fn table_regions_for_page(page: &Page, excluded: &[RegionRect]) -> Vec<MediaRegion> {
    let rows = fragment_rows(page);
    let mut regions = Vec::new();
    let mut group: Vec<&FragmentRow<'_>> = Vec::new();

    for row in &rows {
        if row_overlaps_excluded_region(row, excluded) {
            if !group.is_empty() {
                push_table_region(page, &group, &mut regions);
                group.clear();
            }
            continue;
        }
        if is_tableish_row(row) {
            let continues = group
                .last()
                .is_some_and(|last| row.top - last.bottom <= (last.height().max(12) * 3).max(36));
            if !group.is_empty() && !continues {
                push_table_region(page, &group, &mut regions);
                group.clear();
            }
            group.push(row);
        } else if !group.is_empty() {
            push_table_region(page, &group, &mut regions);
            group.clear();
        }
    }
    if !group.is_empty() {
        push_table_region(page, &group, &mut regions);
    }

    regions
}

fn row_overlaps_excluded_region(row: &FragmentRow<'_>, excluded: &[RegionRect]) -> bool {
    excluded.iter().any(|region| {
        row.fragments
            .iter()
            .any(|fragment| region.overlaps_fragment(fragment))
    })
}

fn push_table_region(page: &Page, rows: &[&FragmentRow<'_>], regions: &mut Vec<MediaRegion>) {
    if rows.len() < 3 || !table_group_has_aligned_columns(rows) {
        return;
    }
    let mut rect = rect_from_rows(page, rows);
    if page_has_two_column_prose(page)
        && let Some((left, right)) = column_bounds_for_table_rows(page, rows)
        && let Some(clamped) = clamp_rect_horizontally(rect, left, right)
    {
        rect = clamped;
    }
    let caption = detect_table_caption(page, rect);
    if rows.len() < 4 && caption.is_none() {
        return;
    }
    regions.push(MediaRegion {
        kind: MediaKind::Table,
        rect,
        caption,
    });
}

fn is_tableish_row(row: &FragmentRow<'_>) -> bool {
    if row.fragments.len() < 3 {
        return false;
    }
    let numeric_cells = row
        .fragments
        .iter()
        .filter(|fragment| {
            let text = spans_text(&fragment.spans);
            text.chars().any(|ch| ch.is_ascii_digit()) || text.contains('%')
        })
        .count();
    let short_cells = row
        .fragments
        .iter()
        .filter(|fragment| spans_text(&fragment.spans).trim().chars().count() <= 32)
        .count();

    numeric_cells >= 2 && short_cells >= row.fragments.len().saturating_sub(1)
}

fn table_group_has_aligned_columns(rows: &[&FragmentRow<'_>]) -> bool {
    let mut buckets: HashMap<i32, usize> = HashMap::new();
    for row in rows {
        let mut seen = Vec::new();
        for fragment in &row.fragments {
            let bucket = fragment.left / 12;
            if !seen.contains(&bucket) {
                seen.push(bucket);
            }
        }
        for bucket in seen {
            *buckets.entry(bucket).or_default() += 1;
        }
    }
    let min_hits = (rows.len() / 2).max(2);
    buckets.values().filter(|hits| **hits >= min_hits).count() >= 3
}

fn rect_from_rows(page: &Page, rows: &[&FragmentRow<'_>]) -> RegionRect {
    let left = rows.iter().map(|row| row.left).min().unwrap_or(0);
    let right = rows.iter().map(|row| row.right).max().unwrap_or(page.width);
    let top = rows.iter().map(|row| row.top).min().unwrap_or(0);
    let bottom = rows
        .iter()
        .map(|row| row.bottom)
        .max()
        .unwrap_or(page.height);
    RegionRect {
        page: page.number,
        top,
        left,
        width: right - left,
        height: bottom - top,
    }
}

fn column_bounds_for_table_rows(page: &Page, rows: &[&FragmentRow<'_>]) -> Option<(i32, i32)> {
    let mut fragments = rows
        .iter()
        .flat_map(|row| row.fragments.iter().copied())
        .filter(|fragment| is_table_signal_fragment(fragment))
        .collect::<Vec<_>>();
    if fragments.is_empty() {
        fragments = rows
            .iter()
            .flat_map(|row| row.fragments.iter().copied())
            .collect::<Vec<_>>();
    }
    column_bounds_for_fragments(page, &fragments)
}

fn is_table_signal_fragment(fragment: &Fragment) -> bool {
    let text = spans_text(&fragment.spans);
    text.chars().any(|ch| ch.is_ascii_digit()) || text.contains('%')
}

fn equation_regions_for_page(page: &Page, excluded: &[RegionRect]) -> Vec<MediaRegion> {
    let mut fragments = page
        .fragments
        .iter()
        .filter(|fragment| {
            !excluded
                .iter()
                .any(|region| region.overlaps_fragment(fragment))
                && is_display_equation_fragment(page, fragment)
        })
        .collect::<Vec<_>>();
    fragments.sort_by_key(|fragment| (fragment.top, fragment.left));

    let mut regions = Vec::new();
    let mut group: Vec<&Fragment> = Vec::new();
    for fragment in fragments {
        let continues = group.last().is_some_and(|last| {
            fragment.top - (last.top + last.height)
                <= (last.height.max(fragment.height) * 2).max(24)
        });
        if !group.is_empty() && !continues {
            push_equation_region(page, &group, &mut regions);
            group.clear();
        }
        group.push(fragment);
    }
    if !group.is_empty() {
        push_equation_region(page, &group, &mut regions);
    }

    regions
}

fn push_equation_region(page: &Page, fragments: &[&Fragment], regions: &mut Vec<MediaRegion>) {
    let rect = rect_from_fragments(page, fragments);
    regions.push(MediaRegion {
        kind: MediaKind::Equation,
        rect,
        caption: None,
    });
}

fn rect_from_fragments(page: &Page, fragments: &[&Fragment]) -> RegionRect {
    let left = fragments
        .iter()
        .map(|fragment| fragment.left)
        .min()
        .unwrap_or(0);
    let right = fragments
        .iter()
        .map(|fragment| fragment.right())
        .max()
        .unwrap_or(page.width);
    let top = fragments
        .iter()
        .map(|fragment| fragment.top)
        .min()
        .unwrap_or(0);
    let bottom = fragments
        .iter()
        .map(|fragment| fragment.top + fragment.height)
        .max()
        .unwrap_or(page.height);
    RegionRect {
        page: page.number,
        top,
        left,
        width: right - left,
        height: bottom - top,
    }
}

fn is_display_equation_fragment(page: &Page, fragment: &Fragment) -> bool {
    let text = spans_text(&fragment.spans);
    let trimmed = text.trim();
    if trimmed.chars().count() < 3 || is_caption_text(trimmed) {
        return false;
    }
    if is_single_parenthetical(trimmed) {
        return false;
    }
    let nonspace = trimmed.chars().filter(|ch| !ch.is_whitespace()).count();
    let math_symbols = trimmed
        .chars()
        .filter(|ch| is_inline_math_operator(*ch))
        .count();
    let word_count = trimmed.split_whitespace().count();
    let centered = ((fragment.left + fragment.width / 2) - page.width / 2).abs() <= page.width / 5;
    let short = fragment.width <= page.width * 7 / 10;
    let has_strong_operator = trimmed.chars().any(is_strong_inline_math_operator);

    centered
        && short
        && has_strong_operator
        && word_count <= 8
        && math_symbols >= 2
        && math_symbols * 3 >= nonspace
}

fn is_single_parenthetical(text: &str) -> bool {
    let Some(inner) = text
        .strip_prefix('(')
        .and_then(|value| value.strip_suffix(')'))
    else {
        return false;
    };
    !inner.contains('(') && !inner.contains(')')
}

fn figure_blocks_from_images(
    pages: &[Page],
    page_stats: &[PageStats],
    images: &[ExtractedImage],
    crop_renderer: &mut PageCropRenderer<'_>,
    output_dir: &Path,
) -> Result<FigureBlocks> {
    let pages_by_number = pages
        .iter()
        .map(|page| (page.number, page))
        .collect::<HashMap<_, _>>();
    let candidates = image_figure_candidates(&pages_by_number, images);
    let repeated_regionless = repeated_regionless_image_signatures(&candidates);
    let two_column_pages = page_stats
        .iter()
        .filter(|stats| stats.two_column)
        .map(|stats| stats.page)
        .collect::<HashSet<_>>();
    let mut warnings = Vec::new();
    let vector_regions = vector_figure_regions(pages, &two_column_pages, &mut warnings);
    let vector_rects = vector_regions
        .iter()
        .filter_map(|region| {
            let page = pages_by_number.get(&region.rect.page).copied()?;
            Some(
                snap_rect_above_caption(padded_vector_crop_rect(page, region.rect), region.caption)
                    .0,
            )
        })
        .collect::<Vec<_>>();
    let mut used_images = vec![false; candidates.len()];
    for (index, candidate) in candidates.iter().enumerate() {
        if candidate_region_rect(candidate)
            .is_some_and(|rect| region_covered_by_any(rect, &vector_rects))
        {
            used_images[index] = true;
        }
    }
    let mut figures = Vec::new();
    let mut exclusions = Vec::new();
    let mut figure_crop_count = 0;

    for cluster in image_candidate_clusters(&candidates, &used_images, &pages_by_number) {
        if cluster.len() < 2 {
            continue;
        }
        let page_number = candidates[cluster[0]].image.page;
        let Some(page) = pages_by_number.get(&page_number).copied() else {
            continue;
        };
        let regions = cluster
            .iter()
            .filter_map(|index| candidate_region_rect(&candidates[*index]))
            .collect::<Vec<_>>();
        let text_rect = rect_from_region_rects(page, &regions);
        let rect = text_rect.padded(page, 8);
        let caption = detect_caption_fragment_for_rect(page, Some(text_rect));
        let (rect, snapped) = caption
            .map(|caption| snap_rect_above_caption(rect, caption))
            .unwrap_or((rect, false));
        figure_crop_count += 1;
        let asset = match render_figure_crop(
            crop_renderer,
            output_dir,
            figure_crop_count,
            rect,
            "pdf-figure",
        ) {
            Ok(asset) => asset,
            Err(err) => {
                warnings.push(format!(
                    "page {}: skipped grouped figure crop near y={} because raster rendering failed: {err}",
                    rect.page, rect.top
                ));
                figure_crop_count -= 1;
                continue;
            }
        };
        if snapped && let Some(caption) = caption {
            warnings.push(caption_snap_warning(rect.page, caption.top));
        }
        figures.push(AnchoredFigure {
            block: DocBlock::Figure {
                image: asset,
                caption: caption.map(|caption| caption.spans.clone()),
            },
            anchor: BlockAnchor {
                page: rect.page,
                top: rect.top,
                left: rect.left,
                width: rect.width,
            },
            text_region: Some(text_rect),
        });
        exclusions.push(text_rect);
        for group_index in cluster {
            used_images[group_index] = true;
        }
    }

    for (index, candidate) in candidates.iter().enumerate() {
        if used_images[index] {
            continue;
        }
        if let (Some(region), Some(caption)) = (candidate.region, candidate.caption)
            && caption_overlaps_image(region, caption)
        {
            warnings.push(caption_overlap_warning(candidate.image.page, caption.top));
        }
        if candidate.region.is_none()
            && should_drop_regionless_image(candidate.image, &repeated_regionless, &mut warnings)
        {
            continue;
        }
        let top = candidate
            .region
            .map(|region| region.top)
            .unwrap_or(i32::MAX);
        let left = candidate.region.map(|region| region.left).unwrap_or(0);
        let width = candidate
            .region
            .map(|region| region.width)
            .or_else(|| candidate.image.width.map(|width| width as i32))
            .unwrap_or(1);
        let asset = image_asset(candidate.image, candidate.region)?;
        figures.push(AnchoredFigure {
            block: DocBlock::Figure {
                image: asset,
                caption: candidate.caption.map(|caption| caption.spans.clone()),
            },
            anchor: BlockAnchor {
                page: candidate.image.page,
                top,
                left,
                width,
            },
            text_region: None,
        });
    }

    for region in vector_regions {
        figure_crop_count += 1;
        let rect = padded_vector_crop_rect(
            pages_by_number
                .get(&region.rect.page)
                .copied()
                .expect("vector figure regions come from known pages"),
            region.rect,
        );
        let (rect, snapped) = snap_rect_above_caption(rect, region.caption);
        let asset = match render_figure_crop(
            crop_renderer,
            output_dir,
            figure_crop_count,
            rect,
            "pdf-figure",
        ) {
            Ok(asset) => asset,
            Err(err) => {
                warnings.push(format!(
                    "page {}: skipped vector figure crop near y={} because raster rendering failed: {err}",
                    rect.page, rect.top
                ));
                figure_crop_count -= 1;
                continue;
            }
        };
        if snapped {
            warnings.push(caption_snap_warning(rect.page, region.caption.top));
        }
        figures.push(AnchoredFigure {
            block: DocBlock::Figure {
                image: asset,
                caption: Some(region.caption.spans.clone()),
            },
            anchor: BlockAnchor {
                page: rect.page,
                top: rect.top,
                left: rect.left,
                width: rect.width,
            },
            text_region: Some(region.rect),
        });
        exclusions.push(region.rect);
    }
    drop_overlapping_uncaptioned_figures(&mut figures);

    Ok(FigureBlocks {
        figures,
        warnings,
        exclusions,
    })
}

fn drop_overlapping_uncaptioned_figures(figures: &mut Vec<AnchoredFigure>) {
    let captioned = figures
        .iter()
        .filter(|figure| {
            matches!(
                &figure.block,
                DocBlock::Figure {
                    caption: Some(_),
                    ..
                }
            )
        })
        .filter_map(anchored_figure_rect)
        .collect::<Vec<_>>();
    figures.retain(|figure| {
        if matches!(
            &figure.block,
            DocBlock::Figure {
                caption: Some(_),
                ..
            }
        ) {
            return true;
        }
        let Some(rect) = anchored_figure_rect(figure) else {
            return true;
        };
        !region_covered_by_any(rect, &captioned)
            && !captioned.iter().any(|captioned| {
                captioned.page == rect.page
                    && rect.top <= captioned.bottom().saturating_add(320)
                    && rect.bottom().saturating_add(320) >= captioned.top
            })
    });
}

fn anchored_figure_rect(figure: &AnchoredFigure) -> Option<RegionRect> {
    let DocBlock::Figure { image, .. } = &figure.block else {
        return None;
    };
    Some(RegionRect {
        page: image.page,
        top: image.top?,
        left: image.left?,
        width: image.width?,
        height: image.height?,
    })
}

const IMAGE_CLUSTER_GAP: i32 = 28;

fn image_candidate_clusters(
    candidates: &[ImageFigureCandidate<'_>],
    already_used: &[bool],
    pages_by_number: &HashMap<u32, &Page>,
) -> Vec<Vec<usize>> {
    let mut visited = already_used.to_vec();
    let caption_keys = candidates
        .iter()
        .map(|candidate| extended_caption_key(candidate, pages_by_number))
        .collect::<Vec<_>>();
    let mut clusters = Vec::new();

    for index in 0..candidates.len() {
        if visited[index] || candidate_region_rect(&candidates[index]).is_none() {
            continue;
        }
        visited[index] = true;
        let mut cluster = vec![index];
        let mut stack = vec![index];

        while let Some(current) = stack.pop() {
            let current_rect =
                candidate_region_rect(&candidates[current]).expect("clustered candidate has rect");
            for other in 0..candidates.len() {
                if visited[other] {
                    continue;
                }
                let Some(other_rect) = candidate_region_rect(&candidates[other]) else {
                    continue;
                };
                if current_rect.touches_within(other_rect, IMAGE_CLUSTER_GAP)
                    || candidates_share_caption(&candidates[current], &candidates[other])
                    || caption_keys[current].is_some()
                        && caption_keys[current] == caption_keys[other]
                {
                    visited[other] = true;
                    stack.push(other);
                    cluster.push(other);
                }
            }
        }

        clusters.push(cluster);
    }

    merge_vertically_overlapping_clusters(clusters, candidates, pages_by_number)
}

fn merge_vertically_overlapping_clusters(
    mut clusters: Vec<Vec<usize>>,
    candidates: &[ImageFigureCandidate<'_>],
    pages_by_number: &HashMap<u32, &Page>,
) -> Vec<Vec<usize>> {
    let mut index = 0;
    while index < clusters.len() {
        let mut other = index + 1;
        while other < clusters.len() {
            if clusters_vertically_overlap(
                &clusters[index],
                &clusters[other],
                candidates,
                pages_by_number,
            ) {
                let mut merged = clusters.remove(other);
                clusters[index].append(&mut merged);
            } else {
                other += 1;
            }
        }
        index += 1;
    }
    clusters
}

fn clusters_vertically_overlap(
    left: &[usize],
    right: &[usize],
    candidates: &[ImageFigureCandidate<'_>],
    pages_by_number: &HashMap<u32, &Page>,
) -> bool {
    let Some(left_rect) = cluster_rect(left, candidates, pages_by_number) else {
        return false;
    };
    let Some(right_rect) = cluster_rect(right, candidates, pages_by_number) else {
        return false;
    };
    left_rect.vertically_overlaps(right_rect)
}

fn cluster_rect(
    cluster: &[usize],
    candidates: &[ImageFigureCandidate<'_>],
    pages_by_number: &HashMap<u32, &Page>,
) -> Option<RegionRect> {
    let page_number = candidates.get(*cluster.first()?)?.image.page;
    let page = pages_by_number.get(&page_number).copied()?;
    let regions = cluster
        .iter()
        .filter_map(|index| candidate_region_rect(&candidates[*index]))
        .collect::<Vec<_>>();
    (!regions.is_empty()).then(|| rect_from_region_rects(page, &regions))
}

fn extended_caption_key(
    candidate: &ImageFigureCandidate<'_>,
    pages_by_number: &HashMap<u32, &Page>,
) -> Option<(u32, i32, String)> {
    let rect = candidate_region_rect(candidate)?;
    let page = pages_by_number.get(&candidate.image.page).copied()?;
    let mut captions = figure_caption_fragments(page);
    captions.sort_by_key(|caption| caption.top);
    captions
        .into_iter()
        .filter(|caption| {
            rect.bottom() <= caption.top && caption.top.saturating_sub(rect.bottom()) <= 320
        })
        .min_by_key(|caption| caption.top.saturating_sub(rect.bottom()))
        .map(|caption| {
            (
                candidate.image.page,
                caption.top,
                normalize_text_key(&spans_text(&caption.spans)),
            )
        })
}

fn candidate_region_rect(candidate: &ImageFigureCandidate<'_>) -> Option<RegionRect> {
    candidate
        .region
        .map(|region| region_rect_from_image_region(candidate.image.page, region))
}

fn candidates_share_caption(
    left: &ImageFigureCandidate<'_>,
    right: &ImageFigureCandidate<'_>,
) -> bool {
    left.image.page == right.image.page
        && left
            .caption
            .zip(right.caption)
            .is_some_and(|(left, right)| {
                normalize_text_key(&spans_text(&left.spans))
                    == normalize_text_key(&spans_text(&right.spans))
            })
}

fn region_covered_by_any(rect: RegionRect, regions: &[RegionRect]) -> bool {
    regions.iter().any(|region| {
        let overlap = rect.overlap_area(*region);
        overlap > 0 && overlap * 5 >= rect.area()
    })
}

fn image_figure_candidates<'a>(
    pages_by_number: &HashMap<u32, &'a Page>,
    images: &'a [ExtractedImage],
) -> Vec<ImageFigureCandidate<'a>> {
    let mut used_regions: HashMap<u32, HashSet<usize>> = HashMap::new();
    let mut candidates = Vec::new();

    for image in images {
        let page = pages_by_number.get(&image.page).copied();
        let region = page.and_then(|page| {
            let used = used_regions.entry(page.number).or_default();
            match_best_image_region(image, page, used).map(|index| {
                used.insert(index);
                &page.images[index]
            })
        });

        candidates.push(ImageFigureCandidate {
            image,
            region,
            caption: page.and_then(|page| detect_caption_fragment(page, region)),
        });
    }

    candidates
}

fn match_best_image_region(
    image: &ExtractedImage,
    page: &Page,
    used: &HashSet<usize>,
) -> Option<usize> {
    let (image_width, image_height) = (image.width?, image.height?);
    if image_width == 0 || image_height == 0 {
        return None;
    }
    let image_aspect = image_width as f64 / image_height as f64;
    page.images
        .iter()
        .enumerate()
        .filter(|(index, region)| !used.contains(index) && region.width > 0 && region.height > 0)
        .filter_map(|(index, region)| {
            let region_aspect = region.width as f64 / region.height as f64;
            let aspect_delta = relative_delta(image_aspect, region_aspect);
            (aspect_delta <= 0.20).then(|| {
                let width_delta = relative_delta(image_width as f64, region.width as f64);
                let height_delta = relative_delta(image_height as f64, region.height as f64);
                let score = aspect_delta * 3.0 + width_delta.min(2.0) + height_delta.min(2.0);
                (index, score)
            })
        })
        .min_by(|(_, left), (_, right)| left.total_cmp(right))
        .map(|(index, _)| index)
}

fn relative_delta(left: f64, right: f64) -> f64 {
    if left <= 0.0 || right <= 0.0 {
        return f64::INFINITY;
    }
    (left - right).abs() / left.max(right)
}

fn repeated_regionless_image_signatures(candidates: &[ImageFigureCandidate<'_>]) -> HashSet<u64> {
    let mut pages_by_signature: HashMap<u64, HashSet<u32>> = HashMap::new();
    for candidate in candidates {
        if candidate.region.is_some() {
            continue;
        }
        if let Some(signature) = image_byte_signature(candidate.image) {
            pages_by_signature
                .entry(signature)
                .or_default()
                .insert(candidate.image.page);
        }
    }
    pages_by_signature
        .into_iter()
        .filter_map(|(signature, pages)| {
            (pages.len() >= REPEATED_IMAGE_PAGE_THRESHOLD).then_some(signature)
        })
        .collect()
}

fn should_drop_regionless_image(
    image: &ExtractedImage,
    repeated_signatures: &HashSet<u64>,
    warnings: &mut Vec<String>,
) -> bool {
    let area = image
        .width
        .unwrap_or(0)
        .saturating_mul(image.height.unwrap_or(0));
    if area < MIN_REGIONLESS_IMAGE_AREA {
        warnings.push(format!(
            "page {}: dropped regionless image {} ({} px^2 below minimum area)",
            image.page, image.index, area
        ));
        return true;
    }
    if image_byte_signature(image).is_some_and(|signature| repeated_signatures.contains(&signature))
    {
        warnings.push(format!(
            "page {}: dropped repeated regionless image {} as likely running ornament/logo",
            image.page, image.index
        ));
        return true;
    }
    false
}

fn image_byte_signature(image: &ExtractedImage) -> Option<u64> {
    let bytes = fs::read(&image.path).ok()?;
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    Some(hasher.finish())
}

fn vector_figure_regions<'a>(
    pages: &'a [Page],
    two_column_pages: &HashSet<u32>,
    warnings: &mut Vec<String>,
) -> Vec<FigureCropRegion<'a>> {
    let mut regions = Vec::new();
    for page in pages {
        for caption in figure_caption_fragments(page) {
            let two_column =
                two_column_pages.contains(&page.number) && page_has_two_column_prose(page);
            let chart_fragments = chart_label_fragments(page, caption, two_column);
            if chart_fragments.len() < 4
                || chart_fragments
                    .iter()
                    .filter(|fragment| {
                        spans_text(&fragment.spans)
                            .chars()
                            .any(|ch| ch.is_ascii_digit())
                    })
                    .count()
                    < 3
            {
                continue;
            }
            let Some(mut rect) = vector_region_rect(page, caption, &chart_fragments, two_column)
            else {
                continue;
            };
            rect = shrink_region_away_from_prose(page, rect, &chart_fragments);
            if chart_fragments
                .iter()
                .filter(|fragment| rect.overlaps_fragment(fragment))
                .count()
                < 4
            {
                warnings.push(format!(
                    "page {}: skipped vector figure near y={} because prose-like text intersects the candidate region",
                    page.number, caption.top
                ));
                continue;
            }
            regions.push(FigureCropRegion { rect, caption });
        }
    }
    regions
}

fn chart_label_fragments<'a>(
    page: &'a Page,
    caption: &Fragment,
    two_column: bool,
) -> Vec<&'a Fragment> {
    let mut fragments = page
        .fragments
        .iter()
        .filter(|fragment| {
            let bottom = fragment.top + fragment.height;
            bottom <= caption.top
                && caption.top.saturating_sub(bottom) <= 360
                && is_chart_label_fragment(fragment)
        })
        .collect::<Vec<_>>();
    let single_column_caption = two_column && !fragment_spans_columns(page, caption);
    if single_column_caption {
        if let Some((left, right)) = column_bounds_for_fragments(page, &[caption]) {
            fragments.retain(|fragment| {
                let center = fragment.left + fragment.width / 2;
                center >= left && center <= right
            });
        }
    } else if two_column
        && !fragments_span_columns(page, &fragments)
        && let Some((left, right)) = column_bounds_for_labels(page, &fragments)
    {
        fragments.retain(|fragment| {
            let center = fragment.left + fragment.width / 2;
            center >= left && center <= right
        });
    }
    if single_column_caption && !fragments_span_columns(page, &fragments) {
        trim_fragments_above_large_vertical_gap(&mut fragments);
    }
    fragments
}

fn is_chart_label_fragment(fragment: &Fragment) -> bool {
    let text = spans_text(&fragment.spans);
    let trimmed = text.trim();
    if trimmed.is_empty() || is_caption_text(trimmed) || is_prose_like_fragment(fragment) {
        return false;
    }
    let chars = trimmed.chars().count();
    let words = trimmed.split_whitespace().count();
    let has_digit = trimmed.chars().any(|ch| ch.is_ascii_digit());
    let short_axis_label = words <= 3 && chars <= 24;
    (has_digit || short_axis_label) && words <= 4 && chars <= 28
}

fn vector_region_rect(
    page: &Page,
    caption: &Fragment,
    chart_fragments: &[&Fragment],
    two_column: bool,
) -> Option<RegionRect> {
    let label_rect = rect_from_fragments(page, chart_fragments);
    let mut rects = vec![label_rect];
    for image in &page.images {
        let image_rect = region_rect_from_image_region(page.number, image);
        if image_rect.bottom() <= caption.top
            && caption.top.saturating_sub(image_rect.bottom()) <= 360
            && image_rect.touches_within(label_rect, 48)
        {
            rects.push(image_rect);
        }
    }
    let mut rect = rect_from_region_rects(page, &rects);
    if two_column
        && !fragment_spans_columns(page, caption)
        && !fragments_span_columns(page, chart_fragments)
    {
        let (left, right) = column_bounds_for_labels(page, chart_fragments)?;
        rect = clamp_rect_horizontally(rect, left, right)?;
    }
    Some(rect)
}

fn trim_fragments_above_large_vertical_gap(fragments: &mut Vec<&Fragment>) {
    if fragments.len() < 2 {
        return;
    }
    fragments.sort_by_key(|fragment| (fragment.top, fragment.left));
    let Some(split_index) = fragments
        .windows(2)
        .enumerate()
        .filter_map(|(index, pair)| {
            let previous_bottom = pair[0].top + pair[0].height;
            let split_index = index + 1;
            (pair[1].top.saturating_sub(previous_bottom) >= 36
                && fragments_have_chart_signal(&fragments[split_index..]))
            .then_some(split_index)
        })
        .next_back()
    else {
        return;
    };
    fragments.drain(0..split_index);
}

fn fragments_have_chart_signal(fragments: &[&Fragment]) -> bool {
    fragments.len() >= 4
        && fragments
            .iter()
            .filter(|fragment| {
                spans_text(&fragment.spans)
                    .chars()
                    .any(|ch| ch.is_ascii_digit())
            })
            .count()
            >= 3
}

fn fragments_span_columns(page: &Page, fragments: &[&Fragment]) -> bool {
    if fragments.len() < 6 {
        return false;
    }
    let Some((content_left, content_right)) = content_bounds(page) else {
        return false;
    };
    let content_width = (content_right - content_left).max(1);
    let mid = content_left + content_width / 2;
    let left_count = fragments
        .iter()
        .filter(|fragment| fragment.left + fragment.width / 2 <= mid)
        .count();
    let right_count = fragments.len().saturating_sub(left_count);
    let fragment_left = fragments
        .iter()
        .map(|fragment| fragment.left)
        .min()
        .unwrap_or(content_left);
    let fragment_right = fragments
        .iter()
        .map(|fragment| fragment.right())
        .max()
        .unwrap_or(content_right);
    left_count >= 3 && right_count >= 3 && fragment_right - fragment_left >= content_width * 2 / 5
}

fn fragment_spans_columns(page: &Page, fragment: &Fragment) -> bool {
    let Some((content_left, content_right)) = content_bounds(page) else {
        return false;
    };
    let content_width = (content_right - content_left).max(1);
    let mid = content_left + content_width / 2;
    fragment.width >= content_width * 2 / 3 || (fragment.left < mid && fragment.right() > mid)
}

fn content_bounds(page: &Page) -> Option<(i32, i32)> {
    let content_left = page.fragments.iter().map(|fragment| fragment.left).min()?;
    let content_right = page
        .fragments
        .iter()
        .map(Fragment::right)
        .max()
        .unwrap_or(page.width);
    Some((content_left, content_right))
}

fn column_bounds_for_labels(page: &Page, chart_fragments: &[&Fragment]) -> Option<(i32, i32)> {
    column_bounds_for_fragments(page, chart_fragments)
}

fn column_bounds_for_fragments(page: &Page, fragments: &[&Fragment]) -> Option<(i32, i32)> {
    let (content_left, content_right) = content_bounds(page)?;
    let content_width = (content_right - content_left).max(1);
    let mid = content_left + content_width / 2;
    let center_sum = fragments
        .iter()
        .map(|fragment| fragment.left + fragment.width / 2)
        .sum::<i32>();
    let centroid = center_sum / i32::try_from(fragments.len()).ok()?.max(1);
    if centroid <= mid {
        Some((content_left, mid))
    } else {
        Some((mid, content_right))
    }
}

fn page_has_two_column_prose(page: &Page) -> bool {
    let Some(content_left) = page.fragments.iter().map(|fragment| fragment.left).min() else {
        return false;
    };
    let content_right = page
        .fragments
        .iter()
        .map(Fragment::right)
        .max()
        .unwrap_or(page.width);
    let mid = content_left + (content_right - content_left).max(1) / 2;
    let mut left = 0;
    let mut right = 0;
    for fragment in page
        .fragments
        .iter()
        .filter(|fragment| is_prose_like_fragment(fragment))
    {
        if fragment.left + fragment.width / 2 <= mid {
            left += 1;
        } else {
            right += 1;
        }
    }
    left >= 2 && right >= 2
}

fn clamp_rect_horizontally(rect: RegionRect, left: i32, right: i32) -> Option<RegionRect> {
    let clamped_left = rect.left.max(left);
    let clamped_right = rect.right().min(right);
    (clamped_right > clamped_left).then_some(RegionRect {
        left: clamped_left,
        width: clamped_right - clamped_left,
        ..rect
    })
}

fn shrink_region_away_from_prose(
    page: &Page,
    mut rect: RegionRect,
    chart_fragments: &[&Fragment],
) -> RegionRect {
    let label_top = chart_fragments
        .iter()
        .map(|fragment| fragment.top)
        .min()
        .unwrap_or(rect.top);
    let label_bottom = chart_fragments
        .iter()
        .map(|fragment| fragment.top + fragment.height)
        .max()
        .unwrap_or(rect.bottom());
    let original_bottom = rect.bottom();
    for fragment in &page.fragments {
        if !rect.overlaps_fragment(fragment) || !is_prose_like_fragment(fragment) {
            continue;
        }
        let fragment_bottom = fragment.top + fragment.height;
        if fragment_bottom <= label_top {
            let new_top = fragment_bottom.saturating_add(2).min(original_bottom - 1);
            rect.height = original_bottom - new_top;
            rect.top = new_top;
        } else if fragment.top >= label_bottom {
            let new_bottom = fragment.top.saturating_sub(2).max(rect.top + 1);
            rect.height = new_bottom - rect.top;
        }
    }
    rect
}

fn padded_vector_crop_rect(page: &Page, rect: RegionRect) -> RegionRect {
    let mut crop = rect.padded(page, 48);
    if crop.top < rect.top {
        let bottom = crop.bottom();
        crop.top = rect.top;
        crop.height = (bottom - crop.top).max(1);
    }
    crop
}

fn is_prose_like_fragment(fragment: &Fragment) -> bool {
    let text = spans_text(&fragment.spans);
    let trimmed = text.trim();
    let words = trimmed.split_whitespace().count();
    words > 6
        && trimmed
            .chars()
            .any(|ch| matches!(ch, '.' | '!' | '?' | ';' | ':'))
        && trimmed.chars().any(|ch| ch.is_alphabetic())
}

fn rect_from_region_rects(page: &Page, regions: &[RegionRect]) -> RegionRect {
    let left = regions.iter().map(|region| region.left).min().unwrap_or(0);
    let right = regions
        .iter()
        .map(|region| region.right())
        .max()
        .unwrap_or(page.width);
    let top = regions.iter().map(|region| region.top).min().unwrap_or(0);
    let bottom = regions
        .iter()
        .map(|region| region.bottom())
        .max()
        .unwrap_or(page.height);
    RegionRect {
        page: page.number,
        top,
        left,
        width: right - left,
        height: bottom - top,
    }
}

fn render_figure_crop(
    crop_renderer: &mut PageCropRenderer<'_>,
    output_dir: &Path,
    index: usize,
    rect: RegionRect,
    prefix: &str,
) -> Result<ImageAsset> {
    let name = format!("{prefix}-{index:04}");
    let rendered = crop_renderer.render_crop(
        PageCrop {
            page: rect.page,
            left: rect.left,
            top: rect.top,
            width: rect.width,
            height: rect.height,
        },
        output_dir,
        &name,
    )?;
    figure_crop_asset(index, rect, &rendered)
}

fn figure_crop_asset(index: usize, rect: RegionRect, path: &Path) -> Result<ImageAsset> {
    let bytes = fs::read(path)?;
    Ok(ImageAsset {
        id: format!("pdf-figure-{index:04}"),
        href: format!("images/pdf-figure-{index:04}.png"),
        media_type: "image/png".to_string(),
        bytes,
        page: rect.page,
        top: Some(rect.top),
        left: Some(rect.left),
        width: Some(rect.width),
        height: Some(rect.height),
    })
}

fn snap_rect_above_caption(rect: RegionRect, caption: &Fragment) -> (RegionRect, bool) {
    let caption_limit = caption.top.saturating_sub(2);
    if rect.bottom() <= caption_limit {
        return (rect, false);
    }
    let bottom = caption_limit.max(rect.top + 1);
    (
        RegionRect {
            height: bottom - rect.top,
            ..rect
        },
        true,
    )
}

fn caption_overlaps_image(region: &ImageRegion, caption: &Fragment) -> bool {
    caption.top < region.bottom() && caption.top + caption.height > region.top
}

fn caption_snap_warning(page: u32, caption_top: i32) -> String {
    format!(
        "page {page}: figure crop snapped above caption near y={caption_top}; review duplicated caption risk"
    )
}

fn caption_overlap_warning(page: u32, caption_top: i32) -> String {
    format!(
        "page {page}: figure caption overlaps image bounds near y={caption_top}; review duplicated caption inside raster"
    )
}

fn figure_caption_fragments(page: &Page) -> Vec<&Fragment> {
    page.fragments
        .iter()
        .filter(|fragment| is_figure_caption_text(&spans_text(&fragment.spans)))
        .collect()
}

fn detect_caption_fragment<'a>(
    page: &'a Page,
    region: Option<&ImageRegion>,
) -> Option<&'a Fragment> {
    let region = region.map(|region| region_rect_from_image_region(page.number, region));
    detect_caption_fragment_for_rect(page, region)
}

fn detect_caption_fragment_for_rect(page: &Page, region: Option<RegionRect>) -> Option<&Fragment> {
    let mut candidates = figure_caption_fragments(page);
    candidates.sort_by_key(|fragment| fragment.top);

    if let Some(region) = region {
        let bottom = region.bottom();
        candidates
            .into_iter()
            .filter(|fragment| fragment.top >= bottom.saturating_sub(8))
            .min_by_key(|fragment| (fragment.top - bottom).abs())
            .filter(|fragment| (fragment.top - bottom).abs() <= 160)
    } else {
        candidates.into_iter().next()
    }
}

fn is_figure_caption_text(text: &str) -> bool {
    let lower = text.trim_start().to_ascii_lowercase();
    lower.starts_with("figure ")
        || lower.starts_with("figure\u{00a0}")
        || lower.starts_with("fig. ")
        || lower.starts_with("fig.\u{00a0}")
        || lower.starts_with("fig ")
}

fn is_caption_text(text: &str) -> bool {
    is_figure_caption_text(text) || is_table_caption_text(text)
}

fn is_table_caption_text(text: &str) -> bool {
    let lower = text.trim_start().to_ascii_lowercase();
    lower.starts_with("table ") || lower.starts_with("table\u{00a0}")
}

fn detect_table_caption(page: &Page, rect: RegionRect) -> Option<Vec<Span>> {
    let mut candidates = page
        .fragments
        .iter()
        .filter(|fragment| is_table_caption_text(&spans_text(&fragment.spans)))
        .collect::<Vec<_>>();
    candidates.sort_by_key(|fragment| fragment.top);

    candidates
        .iter()
        .rev()
        .find(|fragment| {
            let bottom = fragment.top + fragment.height;
            bottom <= rect.top && rect.top.saturating_sub(bottom) <= 140
        })
        .or_else(|| {
            candidates.iter().find(|fragment| {
                fragment.top >= rect.bottom() && fragment.top.saturating_sub(rect.bottom()) <= 140
            })
        })
        .map(|fragment| fragment.spans.clone())
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

fn insert_figure_blocks(
    blocks: &mut Vec<AnchoredBlock>,
    mut figures: Vec<AnchoredFigure>,
    warnings: &mut Vec<String>,
) -> usize {
    figures.sort_by_key(|figure| (figure.anchor.page, figure.anchor.top));
    let mut removed_chars = 0;

    for figure in figures {
        if let Some(region) = figure.text_region {
            removed_chars += remove_blocks_in_region(blocks, region, warnings);
        }
        if let DocBlock::Figure {
            caption: Some(caption),
            ..
        } = &figure.block
        {
            remove_duplicate_caption_block(blocks, figure.anchor.page, &spans_text(caption));
        }

        let insert_at = blocks
            .iter()
            .position(|anchored| {
                anchored.anchor.page > figure.anchor.page
                    || anchored.anchor.page == figure.anchor.page
                        && anchored.anchor.top > figure.anchor.top
            })
            .unwrap_or(blocks.len());
        blocks.insert(
            insert_at,
            AnchoredBlock {
                block: figure.block,
                anchor: figure.anchor,
            },
        );
    }

    removed_chars
}

fn remove_blocks_in_region(
    blocks: &mut Vec<AnchoredBlock>,
    region: RegionRect,
    warnings: &mut Vec<String>,
) -> usize {
    let mut index = 0;
    let mut removed_chars = 0;
    while index < blocks.len() {
        let anchor = blocks[index].anchor;
        if anchor.page == region.page
            && anchor.top >= region.top
            && anchor.top < region.bottom()
            && anchor_overlaps_region_horizontally(anchor, region)
        {
            let text = blocks[index].block.text();
            if is_prose_like_text(&text) {
                warnings.push(format!(
                    "page {}: kept overlapping prose as text near image region y={} ({} chars): {}",
                    region.page,
                    region.top,
                    blocks[index].block.char_count(),
                    audit_snippet(&text)
                ));
                index += 1;
                continue;
            }
            let removed = blocks.remove(index);
            let chars = removed.block.char_count();
            removed_chars += chars;
            if !text.trim().is_empty() {
                warnings.push(format!(
                    "page {}: preserved as image near y={} ({} chars): {}",
                    region.page,
                    region.top,
                    chars,
                    audit_snippet(&text)
                ));
            }
        } else {
            index += 1;
        }
    }
    removed_chars
}

fn is_prose_like_text(text: &str) -> bool {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    normalized.chars().count() >= 60
        && normalized
            .chars()
            .any(|ch| matches!(ch, '.' | '!' | '?' | ';' | ':'))
        && normalized.split_whitespace().count() > 8
        && normalized.chars().any(|ch| ch.is_alphabetic())
}

fn remove_duplicate_caption_block(blocks: &mut Vec<AnchoredBlock>, page: u32, caption: &str) {
    let normalized = normalize_text_key(caption);
    let Some(index) = blocks.iter().position(|anchored| {
        anchored.anchor.page == page
            && matches!(
                &anchored.block,
                DocBlock::Paragraph { .. } | DocBlock::Heading { .. }
            )
            && normalize_text_key(&anchored.block.text()) == normalized
    }) else {
        return;
    };

    blocks.remove(index);
}

fn anchor_overlaps_region_horizontally(anchor: BlockAnchor, region: RegionRect) -> bool {
    anchor.left < region.right() && anchor.left + anchor.width.max(1) > region.left
}

fn audit_snippet(text: &str) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    const LIMIT: usize = 140;
    if normalized.chars().count() <= LIMIT {
        return normalized;
    }
    let mut snippet = normalized.chars().take(LIMIT).collect::<String>();
    snippet.push_str("...");
    snippet
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
    const BERT_FIGURE1_CAPTION_BOUNDARY_XML: &str =
        include_str!("../fixtures/bert_figure1_caption_boundary.xml");
    #[cfg(unix)]
    const BERT_FIGURE4_MULTIPANEL_XML: &str =
        include_str!("../fixtures/bert_figure4_multipanel.xml");
    #[cfg(unix)]
    const BERT_FIGURE5_VECTOR_CHART_XML: &str =
        include_str!("../fixtures/bert_figure5_vector_chart.xml");
    const BERT_PAGE16_VECTOR_CHART_TWOCOL_XML: &str =
        include_str!("../fixtures/bert_page16_vector_chart_twocol.xml");
    const BERT_FIGURE1_TOKEN_STRIP_XML: &str =
        include_str!("../fixtures/bert_figure1_token_strip.xml");
    #[cfg(unix)]
    const BERT_MODEL_PARAMETER_FALSE_POSITIVE_XML: &str =
        include_str!("../fixtures/bert_model_parameter_false_positive.xml");

    fn span(text: &str) -> Span {
        Span {
            text: text.to_string(),
            bold: false,
            italic: false,
        }
    }

    fn fragment(top: i32, left: i32, width: i32, height: i32, text: &str) -> Fragment {
        Fragment {
            top,
            left,
            width,
            height,
            font: 0,
            spans: vec![span(text)],
        }
    }

    fn empty_page(number: u32) -> Page {
        Page {
            number,
            width: 600,
            height: 800,
            fragments: Vec::new(),
            images: Vec::new(),
            font_sizes: HashMap::new(),
        }
    }

    #[cfg(unix)]
    fn write_executable(path: &Path, script: &str) {
        use std::{fs, io::Write, os::unix::fs::PermissionsExt};

        let mut file = fs::File::create(path).expect("tool fixture");
        file.write_all(script.as_bytes()).expect("tool fixture");
        file.sync_all().expect("tool fixture sync");
        drop(file);
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("tool executable");
    }

    #[cfg(unix)]
    fn fake_pdftohtml_with_xml(path: &Path, xml: &str) {
        write_executable(
            path,
            &format!(
                r#"#!/bin/sh
cat <<'XML'
{xml}
XML
"#
            ),
        );
    }

    #[cfg(unix)]
    fn fake_pdftotext_with_text(path: &Path, text: &str) {
        write_executable(
            path,
            &format!(
                r#"#!/bin/sh
cat <<'TEXT'
{text}
TEXT
"#
            ),
        );
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
    fn fake_pdfimages_two(path: &Path) {
        write_executable(
            path,
            r#"#!/bin/sh
if [ "$1" = "-list" ]; then
cat <<'LIST'
page   num  type   width height color comp bpc  enc interp  object ID x-ppi y-ppi size ratio
--------------------------------------------------------------------------------------------
   1     0 image     180    90  rgb     3   8  image  no        12  0    72    72  1K  1.0%
   1     1 image     180    90  rgb     3   8  image  no        13  0    72    72  1K  1.0%
LIST
exit 0
fi
for last do :; done
printf 'fake-panel-a' > "$last-000-000.png"
printf 'fake-panel-b' > "$last-000-001.png"
echo "$last-000-000.png"
echo "$last-000-001.png"
"#,
        );
    }

    #[cfg(unix)]
    fn fake_pdfimages_empty(path: &Path) {
        write_executable(
            path,
            r#"#!/bin/sh
if [ "$1" = "-list" ]; then
cat <<'LIST'
page   num  type   width height color comp bpc  enc interp  object ID x-ppi y-ppi size ratio
--------------------------------------------------------------------------------------------
LIST
fi
"#,
        );
    }

    #[cfg(unix)]
    fn fake_pdftoppm(path: &Path) {
        let fixture = path.with_file_name("pdftoppm.fixture.png");
        crate::tools::write_solid_rgb_png(&fixture, 1600, 2200, [240, 240, 240])
            .expect("pdftoppm PNG fixture");
        write_executable(
            path,
            r#"#!/bin/sh
for last do :; done
script_dir=$(dirname "$0")
cp "$script_dir/pdftoppm.fixture.png" "$last.png"
"#,
        );
    }

    #[test]
    fn remove_blocks_in_region_requires_horizontal_overlap() {
        let mut blocks = vec![
            AnchoredBlock {
                block: DocBlock::Paragraph {
                    spans: vec![span("left table text 2019 2020")],
                },
                anchor: BlockAnchor {
                    page: 1,
                    top: 120,
                    left: 100,
                    width: 180,
                },
            },
            AnchoredBlock {
                block: DocBlock::Paragraph {
                    spans: vec![span("right column prose must remain")],
                },
                anchor: BlockAnchor {
                    page: 1,
                    top: 120,
                    left: 420,
                    width: 160,
                },
            },
        ];
        let mut warnings = Vec::new();

        let removed = remove_blocks_in_region(
            &mut blocks,
            RegionRect {
                page: 1,
                top: 100,
                left: 90,
                width: 220,
                height: 80,
            },
            &mut warnings,
        );

        assert!(removed > 0);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].block.text(), "right column prose must remain");
        assert!(warnings[0].contains("left table text"));
    }

    #[test]
    fn remove_blocks_in_region_keeps_prose_like_blocks_as_text() {
        let mut blocks = vec![AnchoredBlock {
            block: DocBlock::Paragraph {
                spans: vec![span(
                    "This prose sentence should remain translatable even when a table crop overlaps its anchor and geometry.",
                )],
            },
            anchor: BlockAnchor {
                page: 1,
                top: 120,
                left: 100,
                width: 320,
            },
        }];
        let mut warnings = Vec::new();

        let removed = remove_blocks_in_region(
            &mut blocks,
            RegionRect {
                page: 1,
                top: 100,
                left: 90,
                width: 340,
                height: 80,
            },
            &mut warnings,
        );

        assert_eq!(removed, 0);
        assert_eq!(blocks.len(), 1);
        assert!(warnings[0].contains("kept overlapping prose as text"));
        assert!(!warnings[0].contains("preserved as image"));
    }

    #[test]
    fn image_regions_match_by_dimensions_not_position() {
        let page = Page {
            number: 1,
            width: 600,
            height: 800,
            fragments: vec![
                fragment(210, 100, 240, 12, "Figure 1. Wide image."),
                fragment(575, 100, 240, 12, "Figure 2. Tall image."),
            ],
            images: vec![
                ImageRegion {
                    top: 100,
                    left: 100,
                    width: 270,
                    height: 90,
                    src: None,
                },
                ImageRegion {
                    top: 300,
                    left: 100,
                    width: 90,
                    height: 270,
                    src: None,
                },
            ],
            font_sizes: HashMap::new(),
        };
        let pages_by_number = HashMap::from([(1, &page)]);
        let images = vec![
            ExtractedImage {
                page: 1,
                index: 0,
                width: Some(100),
                height: Some(300),
                path: PathBuf::from("tall.png"),
                extension: "png".to_string(),
            },
            ExtractedImage {
                page: 1,
                index: 1,
                width: Some(300),
                height: Some(100),
                path: PathBuf::from("wide.png"),
                extension: "png".to_string(),
            },
        ];

        let candidates = image_figure_candidates(&pages_by_number, &images);

        assert_eq!(candidates[0].region.expect("tall region").top, 300);
        assert_eq!(
            spans_text(&candidates[0].caption.expect("caption").spans),
            "Figure 2. Tall image."
        );
        assert_eq!(candidates[1].region.expect("wide region").top, 100);
    }

    #[test]
    fn regionless_small_and_repeated_images_are_reported_not_emitted() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repeated = b"repeated ornament bytes";
        let mut images = Vec::new();
        let mut pages = Vec::new();
        for page_number in 1..=3 {
            pages.push(empty_page(page_number));
            let path = dir.path().join(format!("repeat-{page_number}.png"));
            fs::write(&path, repeated).expect("image bytes");
            images.push(ExtractedImage {
                page: page_number,
                index: page_number as usize - 1,
                width: Some(200),
                height: Some(200),
                path,
                extension: "png".to_string(),
            });
        }
        pages.push(empty_page(4));
        let small_path = dir.path().join("small.png");
        fs::write(&small_path, b"small").expect("small image bytes");
        images.push(ExtractedImage {
            page: 4,
            index: 3,
            width: Some(20),
            height: Some(20),
            path: small_path,
            extension: "png".to_string(),
        });
        let tools = PopplerTools {
            pdftohtml: PathBuf::from("pdftohtml"),
            pdftotext: PathBuf::from("pdftotext"),
            pdfimages: None,
            pdftoppm: None,
        };
        let page_dir = dir.path().join("pages");
        let mut renderer = PageCropRenderer::new(Path::new("input.pdf"), &tools, &page_dir);

        let result = figure_blocks_from_images(&pages, &[], &images, &mut renderer, dir.path())
            .expect("figure pass");

        assert!(result.figures.is_empty());
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.contains("repeated regionless image"))
        );
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.contains("below minimum area"))
        );
    }

    #[test]
    fn parenthetical_stats_fragment_is_not_display_equation() {
        let page = empty_page(1);
        let stats = fragment(200, 245, 110, 12, "(p = 0.05)");

        assert!(!is_display_equation_fragment(&page, &stats));
    }

    #[test]
    fn mnli_table_header_fragment_is_not_display_equation() {
        let page = empty_page(1);
        let header = fragment(200, 220, 160, 12, "MNLI-(m/mm) 392k");

        assert!(!is_display_equation_fragment(&page, &header));
    }

    #[test]
    fn three_row_aligned_stats_without_caption_are_not_tables() {
        let mut page = empty_page(1);
        page.fragments = vec![
            fragment(100, 100, 40, 12, "Mean"),
            fragment(100, 180, 30, 12, "12"),
            fragment(100, 240, 30, 12, "14"),
            fragment(120, 100, 40, 12, "SD"),
            fragment(120, 180, 30, 12, "3"),
            fragment(120, 240, 30, 12, "4"),
            fragment(140, 100, 40, 12, "N"),
            fragment(140, 180, 30, 12, "20"),
            fragment(140, 240, 30, 12, "21"),
        ];

        assert!(table_regions_for_page(&page, &[]).is_empty());
    }

    #[test]
    fn table_region_clamps_to_table_column_on_two_column_page() {
        let mut page = empty_page(1);
        page.fragments = vec![
            fragment(
                70,
                72,
                220,
                10,
                "Left column prose establishes a normal sentence.",
            ),
            fragment(
                86,
                72,
                220,
                10,
                "Another left column sentence keeps detection stable.",
            ),
            fragment(
                70,
                330,
                220,
                10,
                "Right column prose establishes a normal sentence.",
            ),
            fragment(
                86,
                330,
                220,
                10,
                "Another right column sentence keeps detection stable.",
            ),
            fragment(130, 72, 140, 10, "Table 1. Scores."),
            fragment(170, 72, 40, 10, "Metric"),
            fragment(170, 150, 32, 10, "2019"),
            fragment(170, 220, 32, 10, "2020"),
            fragment(
                170,
                330,
                220,
                10,
                "Right column prose should not enter this crop.",
            ),
            fragment(190, 72, 20, 10, "A"),
            fragment(190, 150, 32, 10, "0.91"),
            fragment(190, 220, 32, 10, "0.93"),
            fragment(
                190,
                330,
                220,
                10,
                "Neighboring body text remains ordinary prose.",
            ),
            fragment(210, 72, 20, 10, "B"),
            fragment(210, 150, 32, 10, "0.81"),
            fragment(210, 220, 32, 10, "0.84"),
            fragment(
                210,
                330,
                220,
                10,
                "The crop must stay within the table column.",
            ),
        ];

        let regions = table_regions_for_page(&page, &[]);

        assert_eq!(regions.len(), 1);
        let rect = regions[0].rect;
        assert!(
            rect.right() <= 330,
            "table crop must stay in the table column: {rect:?}"
        );
        assert!(
            page.fragments
                .iter()
                .filter(|fragment| fragment.left > page.width / 2)
                .all(|fragment| !rect.overlaps_fragment(fragment)),
            "right-column prose must stay outside the table crop: {rect:?}"
        );
    }

    #[test]
    fn figure_token_rows_inside_image_region_are_not_tables() {
        let pages = parse_pdf2xml(BERT_FIGURE1_TOKEN_STRIP_XML).expect("fixture parses");
        let regions = detect_media_regions(&pages, &[]);

        assert!(
            regions.iter().all(|region| region.kind != MediaKind::Table),
            "token rows inside Figure 1 image bounds must not become table crops"
        );
    }

    #[test]
    fn vector_chart_region_clamps_to_chart_column_and_excludes_prose() {
        let pages = parse_pdf2xml(BERT_PAGE16_VECTOR_CHART_TWOCOL_XML).expect("fixture parses");
        let page = &pages[0];
        let mut warnings = Vec::new();
        let regions = vector_figure_regions(&pages, &HashSet::from([page.number]), &mut warnings);

        assert_eq!(regions.len(), 1);
        let rect = regions[0].rect;
        assert!(
            rect.right() <= page.width / 2,
            "two-column chart region must stay in the caption column: {rect:?}"
        );
        assert!(
            page.fragments
                .iter()
                .filter(|fragment| is_prose_like_fragment(fragment))
                .all(|fragment| !rect.overlaps_fragment(fragment)),
            "prose fragments must stay outside vector chart text region: {rect:?}"
        );
        let crop = padded_vector_crop_rect(page, rect);
        assert_eq!(
            crop.top, rect.top,
            "vector crop must not pad upward into prose above the chart"
        );
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    }

    #[test]
    fn vector_chart_region_drops_short_prose_above_chart_labels() {
        let mut page = empty_page(1);
        page.width = 892;
        page.height = 1262;
        page.fragments = vec![
            fragment(
                100,
                108,
                327,
                15,
                "Left column prose establishes normal text for detection.",
            ),
            fragment(
                120,
                108,
                327,
                15,
                "Another left column sentence keeps this page two-column.",
            ),
            fragment(
                100,
                461,
                327,
                15,
                "Right column prose establishes normal text for detection.",
            ),
            fragment(
                120,
                461,
                327,
                15,
                "Another right column sentence keeps this page two-column.",
            ),
            fragment(711, 108, 71, 15, "In Section"),
            fragment(
                731,
                108,
                327,
                15,
                "mixed strategy for masking the target tokens when",
            ),
            fragment(812, 108, 66, 15, "strategies."),
            fragment(871, 130, 12, 9, "84"),
            fragment(904, 130, 12, 9, "82"),
            fragment(936, 130, 12, 9, "80"),
            fragment(1026, 188, 18, 9, "200"),
            fragment(1043, 205, 130, 9, "Pre-training Steps"),
            fragment(
                1075,
                108,
                327,
                13,
                "Figure 5: Ablation over number of training steps.",
            ),
        ];
        let mut warnings = Vec::new();
        let pages = vec![page];

        let regions = vector_figure_regions(&pages, &HashSet::from([1]), &mut warnings);

        assert_eq!(regions.len(), 1);
        assert!(
            regions[0].rect.top >= 871,
            "chart region must start at chart labels, not short prose: {:?}",
            regions[0].rect
        );
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    }

    #[test]
    fn full_width_vector_diagram_is_not_clamped_to_one_column() {
        let mut page = empty_page(1);
        page.width = 892;
        page.height = 1262;
        page.fragments = vec![
            fragment(
                399,
                108,
                327,
                15,
                "Left column prose establishes normal text for detection.",
            ),
            fragment(
                419,
                108,
                327,
                15,
                "Another left column sentence keeps this page two-column.",
            ),
            fragment(
                399,
                461,
                327,
                15,
                "Right column prose establishes normal text for detection.",
            ),
            fragment(
                419,
                461,
                327,
                15,
                "Another right column sentence keeps this page two-column.",
            ),
            fragment(101, 167, 79, 13, "BERT (Ours)"),
            fragment(153, 144, 12, 6, "Trm"),
            fragment(153, 178, 12, 6, "Trm"),
            fragment(127, 147, 6, 6, "T"),
            fragment(131, 153, 3, 4, "1"),
            fragment(127, 182, 4, 6, "T"),
            fragment(131, 186, 3, 4, "2"),
            fragment(102, 335, 79, 13, "OpenAI GPT"),
            fragment(153, 322, 12, 6, "Trm"),
            fragment(153, 357, 12, 6, "Trm"),
            fragment(127, 326, 6, 6, "T"),
            fragment(131, 332, 3, 4, "1"),
            fragment(105, 608, 36, 13, "ELMo"),
            fragment(165, 486, 15, 6, "Lstm"),
            fragment(165, 683, 15, 6, "Lstm"),
            fragment(165, 749, 15, 6, "Lstm"),
            fragment(127, 573, 6, 6, "T"),
            fragment(131, 579, 3, 4, "1"),
            fragment(127, 608, 4, 6, "T"),
            fragment(131, 612, 3, 4, "2"),
            fragment(228, 678, 6, 6, "E"),
            fragment(233, 685, 3, 4, "N"),
            fragment(
                276,
                108,
                680,
                13,
                "Figure 3: Differences in pre-training model architectures.",
            ),
        ];
        let mut warnings = Vec::new();
        let page_width = page.width;
        let pages = vec![page];

        let regions = vector_figure_regions(&pages, &HashSet::from([1]), &mut warnings);

        assert_eq!(regions.len(), 1);
        assert!(
            regions[0].rect.right() > page_width * 3 / 4,
            "full-width vector diagram must include the rightmost panel: {:?}",
            regions[0].rect
        );
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    }

    #[test]
    fn vertically_overlapping_outer_subimage_clusters_merge() {
        let mut page = empty_page(1);
        page.images = vec![
            ImageRegion {
                top: 120,
                left: 60,
                width: 40,
                height: 30,
                src: None,
            },
            ImageRegion {
                top: 124,
                left: 180,
                width: 40,
                height: 30,
                src: None,
            },
            ImageRegion {
                top: 126,
                left: 235,
                width: 40,
                height: 30,
                src: None,
            },
            ImageRegion {
                top: 122,
                left: 430,
                width: 40,
                height: 30,
                src: None,
            },
        ];
        let pages_by_number = HashMap::from([(1, &page)]);
        let images = (0..4)
            .map(|index| ExtractedImage {
                page: 1,
                index,
                width: Some(40),
                height: Some(30),
                path: PathBuf::from(format!("subimage-{index}.png")),
                extension: "png".to_string(),
            })
            .collect::<Vec<_>>();
        let candidates = image_figure_candidates(&pages_by_number, &images);
        let clusters = image_candidate_clusters(
            &candidates,
            &vec![false; candidates.len()],
            &pages_by_number,
        );

        assert_eq!(
            clusters.len(),
            1,
            "expected one merged cluster: {clusters:?}"
        );
        let mut cluster = clusters[0].clone();
        cluster.sort_unstable();
        assert_eq!(cluster, vec![0, 1, 2, 3]);
    }

    #[cfg(unix)]
    #[test]
    fn nearby_raster_subimages_cluster_into_one_figure() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut page = empty_page(1);
        page.images = vec![
            ImageRegion {
                top: 120,
                left: 100,
                width: 40,
                height: 30,
                src: None,
            },
            ImageRegion {
                top: 124,
                left: 160,
                width: 42,
                height: 30,
                src: None,
            },
            ImageRegion {
                top: 126,
                left: 222,
                width: 40,
                height: 30,
                src: None,
            },
        ];
        page.fragments = vec![fragment(
            190,
            100,
            320,
            12,
            "Figure 1. A diagram composed of raster sub-images.",
        )];
        let mut images = Vec::new();
        for index in 0..3 {
            let path = dir.path().join(format!("subimage-{index}.png"));
            fs::write(&path, format!("subimage-{index}")).expect("image bytes");
            images.push(ExtractedImage {
                page: 1,
                index,
                width: Some(40),
                height: Some(30),
                path,
                extension: "png".to_string(),
            });
        }
        let pdftoppm = dir.path().join("pdftoppm");
        fake_pdftoppm(&pdftoppm);
        let tools = PopplerTools {
            pdftohtml: PathBuf::from("pdftohtml"),
            pdftotext: PathBuf::from("pdftotext"),
            pdfimages: None,
            pdftoppm: Some(pdftoppm),
        };
        let page_dir = dir.path().join("pages");
        let mut renderer = PageCropRenderer::new(Path::new("input.pdf"), &tools, &page_dir);

        let result = figure_blocks_from_images(&[page], &[], &images, &mut renderer, dir.path())
            .expect("figure pass");

        assert_eq!(result.figures.len(), 1);
        assert_eq!(result.exclusions.len(), 1);
        let DocBlock::Figure { image, caption } = &result.figures[0].block else {
            panic!("cluster should emit a figure block");
        };
        assert_eq!(image.id, "pdf-figure-0001");
        assert_eq!(
            caption.as_ref().map(|spans| spans_text(spans)).as_deref(),
            Some("Figure 1. A diagram composed of raster sub-images.")
        );
    }

    #[test]
    fn caption_ranking_prefers_closer_caption_below_image_bottom() {
        let mut page = empty_page(1);
        page.fragments = vec![
            fragment(193, 100, 220, 12, "Figure 1. Above decoy."),
            fragment(202, 100, 220, 12, "Figure 2. True caption."),
        ];
        let region = ImageRegion {
            top: 100,
            left: 100,
            width: 100,
            height: 100,
            src: None,
        };

        let caption = detect_caption_fragment(&page, Some(&region)).expect("caption");

        assert_eq!(spans_text(&caption.spans), "Figure 2. True caption.");
    }

    #[cfg(unix)]
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
        let pdftoppm = dir.path().join("pdftoppm");
        fake_pdftoppm(&pdftoppm);

        let tools = PopplerTools {
            pdftohtml,
            pdftotext,
            pdfimages: Some(pdfimages),
            pdftoppm: Some(pdftoppm),
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
    fn convert_pdf_continues_text_only_without_optional_image_tools() {
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
    <fontspec id="0" size="12" family="Times" color="#000000"/>
    <text top="100" left="100" width="180" height="12" font="0">Text only PDF.</text>
  </page>
</pdf2xml>
XML
"##,
        );
        let pdftotext = dir.path().join("pdftotext");
        fake_pdftotext_with_text(&pdftotext, "Text only PDF.\n");
        let tools = PopplerTools {
            pdftohtml,
            pdftotext,
            pdfimages: None,
            pdftoppm: None,
        };

        let outcome = convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), &tools)
            .expect("conversion should succeed without optional image tools");

        assert!(output.exists());
        assert!(
            outcome
                .report
                .warnings
                .iter()
                .any(|warning| warning.contains("image extraction unavailable"))
        );
        let mut archive =
            ZipArchive::new(fs::File::open(&output).expect("epub opens")).expect("zip opens");
        let mut content = String::new();
        archive
            .by_name("content.xhtml")
            .expect("content exists")
            .read_to_string(&mut content)
            .expect("content reads");
        assert!(content.contains("Text only PDF."));
    }

    #[cfg(unix)]
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
        let pdftoppm = dir.path().join("pdftoppm");
        fake_pdftoppm(&pdftoppm);

        let tools = PopplerTools {
            pdftohtml,
            pdftotext,
            pdfimages: Some(pdfimages),
            pdftoppm: Some(pdftoppm),
        };
        let outcome = convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), &tools)
            .expect("conversion should succeed");

        assert_eq!(outcome.report.images, 1);
        assert_eq!(outcome.report.figures, 1, "{}", outcome.report.summary());

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

    #[cfg(unix)]
    #[cfg(unix)]
    #[test]
    fn convert_pdf_snaps_figure_crop_above_caption_boundary_fixture() {
        use std::{fs, io::Read};
        use zip::ZipArchive;

        let dir = tempfile::tempdir().expect("temp dir");
        let input = dir.path().join("input.pdf");
        let output = dir.path().join("output.epub");
        fs::write(&input, b"dummy pdf").expect("input pdf fixture");

        let pdftohtml = dir.path().join("pdftohtml");
        fake_pdftohtml_with_xml(&pdftohtml, BERT_FIGURE1_CAPTION_BOUNDARY_XML);
        let pdftotext = dir.path().join("pdftotext");
        fake_pdftotext_with_text(
            &pdftotext,
            "Results introduce a boundary-sensitive figure.\nFigure 1. Boundary-sensitive raster caption.\nThe caption should be translated once.\n",
        );
        let pdfimages = dir.path().join("pdfimages");
        fake_pdfimages_two(&pdfimages);
        let pdftoppm = dir.path().join("pdftoppm");
        fake_pdftoppm(&pdftoppm);

        let tools = PopplerTools {
            pdftohtml,
            pdftotext,
            pdfimages: Some(pdfimages),
            pdftoppm: Some(pdftoppm),
        };
        let outcome = convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), &tools)
            .expect("conversion should succeed");

        assert_eq!(outcome.report.images, 2);
        assert_eq!(outcome.report.figures, 1, "{}", outcome.report.summary());
        assert!(
            outcome
                .report
                .warnings
                .iter()
                .any(|warning| warning.contains("figure crop snapped above caption")),
            "caption boundary crop should report the snap"
        );

        let mut archive =
            ZipArchive::new(fs::File::open(&output).expect("epub opens")).expect("zip opens");
        let mut content = String::new();
        archive
            .by_name("content.xhtml")
            .expect("content exists")
            .read_to_string(&mut content)
            .expect("content reads");
        assert!(content.contains("<figure id=\"pdf-figure-0001\">"));
        assert_eq!(
            content
                .matches("Figure 1. Boundary-sensitive raster caption.")
                .count(),
            1
        );

        let mut image = Vec::new();
        archive
            .by_name("images/pdf-figure-0001.png")
            .expect("figure crop exists")
            .read_to_end(&mut image)
            .expect("crop reads");
        assert!(image.starts_with(b"\x89PNG\r\n\x1a\n"));
    }

    #[cfg(unix)]
    #[cfg(unix)]
    #[test]
    fn convert_pdf_groups_multipanel_figure_fixture_as_one_captioned_crop() {
        use std::{fs, io::Read};
        use zip::ZipArchive;

        let dir = tempfile::tempdir().expect("temp dir");
        let input = dir.path().join("input.pdf");
        let output = dir.path().join("output.epub");
        fs::write(&input, b"dummy pdf").expect("input pdf fixture");

        let pdftohtml = dir.path().join("pdftohtml");
        fake_pdftohtml_with_xml(&pdftohtml, BERT_FIGURE4_MULTIPANEL_XML);
        let pdftotext = dir.path().join("pdftotext");
        fake_pdftotext_with_text(
            &pdftotext,
            "A multi-panel result follows.\nFigure 4. Multi-panel activation maps.\nDiscussion resumes after the figure.\n",
        );
        let pdfimages = dir.path().join("pdfimages");
        fake_pdfimages_two(&pdfimages);
        let pdftoppm = dir.path().join("pdftoppm");
        fake_pdftoppm(&pdftoppm);

        let tools = PopplerTools {
            pdftohtml,
            pdftotext,
            pdfimages: Some(pdfimages),
            pdftoppm: Some(pdftoppm),
        };
        let outcome = convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), &tools)
            .expect("conversion should succeed");

        assert_eq!(outcome.report.images, 2);
        assert_eq!(outcome.report.figures, 1, "{}", outcome.report.summary());

        let mut archive =
            ZipArchive::new(fs::File::open(&output).expect("epub opens")).expect("zip opens");
        let mut content = String::new();
        archive
            .by_name("content.xhtml")
            .expect("content exists")
            .read_to_string(&mut content)
            .expect("content reads");
        assert!(content.contains("<figure id=\"pdf-figure-0001\">"));
        assert!(!content.contains("pdf-image-0001"));
        assert!(!content.contains("pdf-image-0002"));
        assert_eq!(
            content
                .matches("Figure 4. Multi-panel activation maps.")
                .count(),
            1
        );
        assert!(content.contains("Discussion resumes after the figure."));
    }

    #[cfg(unix)]
    #[cfg(unix)]
    #[test]
    fn convert_pdf_preserves_vector_chart_fixture_as_captioned_crop() {
        use std::{fs, io::Read};
        use zip::ZipArchive;

        let dir = tempfile::tempdir().expect("temp dir");
        let input = dir.path().join("input.pdf");
        let output = dir.path().join("output.epub");
        fs::write(&input, b"dummy pdf").expect("input pdf fixture");

        let pdftohtml = dir.path().join("pdftohtml");
        fake_pdftohtml_with_xml(&pdftohtml, BERT_FIGURE5_VECTOR_CHART_XML);
        let pdftotext = dir.path().join("pdftotext");
        fake_pdftotext_with_text(
            &pdftotext,
            "Vector-chart results are below.\n1.0 0.5 0.0 0 10 20 Epoch\nFigure 5. Vector chart of held-out accuracy.\nThe next paragraph should stay prose.\n",
        );
        let pdfimages = dir.path().join("pdfimages");
        fake_pdfimages_empty(&pdfimages);
        let pdftoppm = dir.path().join("pdftoppm");
        fake_pdftoppm(&pdftoppm);

        let tools = PopplerTools {
            pdftohtml,
            pdftotext,
            pdfimages: Some(pdfimages),
            pdftoppm: Some(pdftoppm),
        };
        let outcome = convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), &tools)
            .expect("conversion should succeed");

        assert_eq!(outcome.report.images, 0);
        assert_eq!(outcome.report.figures, 1, "{}", outcome.report.summary());
        assert_eq!(outcome.report.tables, 0);
        assert_eq!(outcome.report.equations, 0);

        let mut archive =
            ZipArchive::new(fs::File::open(&output).expect("epub opens")).expect("zip opens");
        let mut content = String::new();
        archive
            .by_name("content.xhtml")
            .expect("content exists")
            .read_to_string(&mut content)
            .expect("content reads");
        assert!(content.contains("<figure id=\"pdf-figure-0001\">"));
        assert_eq!(
            content
                .matches("Figure 5. Vector chart of held-out accuracy.")
                .count(),
            1
        );
        assert!(content.contains("The next paragraph should stay prose."));
        assert!(!content.contains("1.0 0.5"));
        assert!(!content.contains("Epoch"));
    }

    #[cfg(unix)]
    #[cfg(unix)]
    #[test]
    fn convert_pdf_warns_on_lowercase_continuation_after_media() {
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
    <text top="80" left="100" width="260" height="12" font="0">This paragraph</text>
    <image top="130" left="120" width="120" height="80" src="paper-1_1.png"/>
    <text top="260" left="100" width="260" height="12" font="0">continues after the figure.</text>
  </page>
</pdf2xml>
XML
"##,
        );

        let pdftotext = dir.path().join("pdftotext");
        write_executable(
            &pdftotext,
            r#"#!/bin/sh
printf 'This paragraph\ncontinues after the figure.\n'
"#,
        );
        let pdfimages = dir.path().join("pdfimages");
        fake_pdfimages(&pdfimages);
        let pdftoppm = dir.path().join("pdftoppm");
        fake_pdftoppm(&pdftoppm);

        let tools = PopplerTools {
            pdftohtml,
            pdftotext,
            pdfimages: Some(pdfimages),
            pdftoppm: Some(pdftoppm),
        };
        let outcome = convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), &tools)
            .expect("conversion should succeed");

        assert!(
            outcome
                .report
                .warnings
                .iter()
                .any(|warning| warning
                    .contains("lowercase paragraph continuation follows media block")),
            "media-separated lowercase continuations should be flagged"
        );
    }

    #[cfg(unix)]
    #[cfg(unix)]
    #[test]
    fn convert_pdf_preserves_detected_table_as_crop_with_caption() {
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
    <fontspec id="0" size="12" family="Times" color="#000000"/>
    <text top="80" left="100" width="260" height="12" font="0">Body before table.</text>
    <text top="120" left="100" width="220" height="12" font="0">Table 1. Scores.</text>
    <text top="170" left="100" width="50" height="12" font="0">Metric</text>
    <text top="170" left="240" width="40" height="12" font="0">2019</text>
    <text top="170" left="360" width="40" height="12" font="0">2020</text>
    <text top="190" left="100" width="20" height="12" font="0">A</text>
    <text top="190" left="240" width="40" height="12" font="0">0.91</text>
    <text top="190" left="360" width="40" height="12" font="0">0.93</text>
    <text top="210" left="100" width="20" height="12" font="0">B</text>
    <text top="210" left="240" width="40" height="12" font="0">0.81</text>
    <text top="210" left="360" width="40" height="12" font="0">0.84</text>
    <text top="280" left="100" width="260" height="12" font="0">Body after table.</text>
  </page>
</pdf2xml>
XML
"##,
        );

        let pdftotext = dir.path().join("pdftotext");
        write_executable(
            &pdftotext,
            r#"#!/bin/sh
printf 'Body before table.\nTable 1. Scores.\nMetric 2019 2020\nA 0.91 0.93\nB 0.81 0.84\nBody after table.\n'
"#,
        );
        let pdfimages = dir.path().join("pdfimages");
        fake_pdfimages_empty(&pdfimages);
        let pdftoppm = dir.path().join("pdftoppm");
        fake_pdftoppm(&pdftoppm);

        let tools = PopplerTools {
            pdftohtml,
            pdftotext,
            pdfimages: Some(pdfimages),
            pdftoppm: Some(pdftoppm),
        };
        let outcome = convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), &tools)
            .expect("conversion should succeed");

        assert_eq!(outcome.report.tables, 1);
        assert_eq!(outcome.report.equations, 0);
        assert_eq!(outcome.report.figures, 1);
        assert!(outcome.report.media_preserved_chars > 0);
        assert_eq!(outcome.report.coverage_percent, 100.0);
        assert!(
            outcome
                .report
                .warnings
                .iter()
                .any(|warning| warning.contains("preserved as image near y="))
        );
        assert!(
            !outcome
                .report
                .warnings
                .iter()
                .any(|warning| warning.contains("reconstructed text covers only"))
        );

        let mut archive =
            ZipArchive::new(fs::File::open(&output).expect("epub opens")).expect("zip opens");
        let mut content = String::new();
        archive
            .by_name("content.xhtml")
            .expect("content exists")
            .read_to_string(&mut content)
            .expect("content reads");
        assert!(content.contains("Body before table."));
        assert!(content.contains("Body after table."));
        assert!(content.contains("<figure id=\"pdf-table-0001\">"));
        assert!(content.contains("<figcaption>Table 1. Scores.</figcaption>"));
        assert_eq!(content.matches("Table 1. Scores.").count(), 1);
        assert!(!content.contains("0.91"));
        assert!(!content.contains("2019"));

        let mut image = Vec::new();
        archive
            .by_name("images/pdf-table-0001.png")
            .expect("table crop exists")
            .read_to_end(&mut image)
            .expect("image reads");
        assert!(image.starts_with(b"\x89PNG\r\n\x1a\n"));
    }

    #[cfg(unix)]
    #[cfg(unix)]
    #[test]
    fn convert_pdf_preserves_display_equation_as_crop() {
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
    <fontspec id="0" size="12" family="Times" color="#000000"/>
    <text top="80" left="100" width="260" height="12" font="0">Body before equation.</text>
    <text top="160" left="240" width="120" height="12" font="0">E = mc^2</text>
    <text top="230" left="100" width="260" height="12" font="0">Body after equation.</text>
  </page>
</pdf2xml>
XML
"##,
        );

        let pdftotext = dir.path().join("pdftotext");
        write_executable(
            &pdftotext,
            r#"#!/bin/sh
printf 'Body before equation.\nE = mc^2\nBody after equation.\n'
"#,
        );
        let pdfimages = dir.path().join("pdfimages");
        fake_pdfimages_empty(&pdfimages);
        let pdftoppm = dir.path().join("pdftoppm");
        fake_pdftoppm(&pdftoppm);

        let tools = PopplerTools {
            pdftohtml,
            pdftotext,
            pdfimages: Some(pdfimages),
            pdftoppm: Some(pdftoppm),
        };
        let outcome = convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), &tools)
            .expect("conversion should succeed");

        assert_eq!(outcome.report.tables, 0);
        assert_eq!(outcome.report.equations, 1);
        assert_eq!(outcome.report.figures, 1);

        let mut archive =
            ZipArchive::new(fs::File::open(&output).expect("epub opens")).expect("zip opens");
        let mut content = String::new();
        archive
            .by_name("content.xhtml")
            .expect("content exists")
            .read_to_string(&mut content)
            .expect("content reads");
        assert!(content.contains("Body before equation."));
        assert!(content.contains("Body after equation."));
        assert!(content.contains("<figure id=\"pdf-equation-0001\">"));
        assert!(content.contains("images/pdf-equation-0001.png"));
        assert!(!content.contains("E = mc^2"));

        let mut image = Vec::new();
        archive
            .by_name("images/pdf-equation-0001.png")
            .expect("equation crop exists")
            .read_to_end(&mut image)
            .expect("image reads");
        assert!(image.starts_with(b"\x89PNG\r\n\x1a\n"));
    }

    #[cfg(unix)]
    #[cfg(unix)]
    #[test]
    fn convert_pdf_marks_inline_math_as_protected_span() {
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
    <text top="100" left="100" width="360" height="12" font="0">The energy term E = mc^2 appears inline.</text>
  </page>
</pdf2xml>
XML
"##,
        );

        let pdftotext = dir.path().join("pdftotext");
        write_executable(
            &pdftotext,
            r#"#!/bin/sh
printf 'The energy term E = mc^2 appears inline.\n'
"#,
        );
        let pdfimages = dir.path().join("pdfimages");
        fake_pdfimages_empty(&pdfimages);
        let pdftoppm = dir.path().join("pdftoppm");
        fake_pdftoppm(&pdftoppm);

        let tools = PopplerTools {
            pdftohtml,
            pdftotext,
            pdfimages: Some(pdfimages),
            pdftoppm: Some(pdftoppm),
        };
        convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), &tools)
            .expect("conversion should succeed");

        let book = bookforge_epub::read_epub(&output).expect("converted EPUB should be readable");
        assert!(
            book.blocks.iter().any(|block| {
                block.protected_spans.iter().any(|span| {
                    span.kind == bookforge_core::ir::ProtectedSpanKind::Math
                        && span.text == "E = mc^2"
                })
            }),
            "inline math should become a protected span after PDF conversion"
        );
    }

    #[cfg(unix)]
    #[cfg(unix)]
    #[test]
    fn convert_pdf_does_not_crop_model_parameter_prose_as_equation() {
        use std::{fs, io::Read};
        use zip::ZipArchive;

        let dir = tempfile::tempdir().expect("temp dir");
        let input = dir.path().join("input.pdf");
        let output = dir.path().join("output.epub");
        fs::write(&input, b"dummy pdf").expect("input pdf fixture");

        let pdftohtml = dir.path().join("pdftohtml");
        fake_pdftohtml_with_xml(&pdftohtml, BERT_MODEL_PARAMETER_FALSE_POSITIVE_XML);

        let pdftotext = dir.path().join("pdftotext");
        fake_pdftotext_with_text(
            &pdftotext,
            "The model = stable result used k = 3 in parentheses (n = 12).\n",
        );
        let pdfimages = dir.path().join("pdfimages");
        fake_pdfimages_empty(&pdfimages);
        let pdftoppm = dir.path().join("pdftoppm");
        fake_pdftoppm(&pdftoppm);

        let tools = PopplerTools {
            pdftohtml,
            pdftotext,
            pdfimages: Some(pdfimages),
            pdftoppm: Some(pdftoppm),
        };
        let outcome = convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), &tools)
            .expect("conversion should succeed");

        assert_eq!(outcome.report.tables, 0);
        assert_eq!(outcome.report.equations, 0);
        assert_eq!(outcome.report.figures, 0);

        let mut archive =
            ZipArchive::new(fs::File::open(&output).expect("epub opens")).expect("zip opens");
        let mut content = String::new();
        archive
            .by_name("content.xhtml")
            .expect("content exists")
            .read_to_string(&mut content)
            .expect("content reads");
        assert!(content.contains("The model = stable result used k = 3"));
        assert!(!content.contains("pdf-equation-0001.png"));
    }

    #[cfg(unix)]
    #[cfg(unix)]
    #[test]
    fn low_confidence_pages_linearize_by_default() {
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
    <fontspec id="0" size="12" family="Times" color="#000000"/>
    <text top="100" left="100" width="20" height="12" font="0">Tiny</text>
  </page>
</pdf2xml>
XML
"##,
        );

        let pdftotext = dir.path().join("pdftotext");
        write_executable(
            &pdftotext,
            r#"#!/bin/sh
printf 'Tiny plus many baseline characters that the XML reconstruction did not recover.\n'
"#,
        );
        let pdfimages = dir.path().join("pdfimages");
        fake_pdfimages_empty(&pdfimages);
        let pdftoppm = dir.path().join("pdftoppm");
        fake_pdftoppm(&pdftoppm);

        let tools = PopplerTools {
            pdftohtml,
            pdftotext,
            pdfimages: Some(pdfimages),
            pdftoppm: Some(pdftoppm),
        };
        let outcome = convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), &tools)
            .expect("conversion should succeed");

        assert_eq!(outcome.report.low_confidence_pages, 1);
        assert_eq!(
            outcome.report.page_stats[0]
                .low_confidence_action
                .as_deref(),
            Some("linearize")
        );
        assert!(
            outcome
                .report
                .warnings
                .iter()
                .any(|warning| warning.contains("page 1: low-confidence")
                    && warning.contains("action=linearize"))
        );

        let mut archive =
            ZipArchive::new(fs::File::open(&output).expect("epub opens")).expect("zip opens");
        let mut content = String::new();
        archive
            .by_name("content.xhtml")
            .expect("content exists")
            .read_to_string(&mut content)
            .expect("content reads");
        assert!(content.contains("Tiny"));
        assert!(!content.contains("pdf-page-0001.png"));
    }

    #[cfg(unix)]
    #[cfg(unix)]
    #[test]
    fn low_confidence_pages_can_be_preserved_as_page_images() {
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
    <fontspec id="0" size="12" family="Times" color="#000000"/>
    <text top="100" left="100" width="20" height="12" font="0">Tiny</text>
  </page>
</pdf2xml>
XML
"##,
        );

        let pdftotext = dir.path().join("pdftotext");
        write_executable(
            &pdftotext,
            r#"#!/bin/sh
printf 'Tiny plus many baseline characters that the XML reconstruction did not recover.\n'
"#,
        );
        let pdfimages = dir.path().join("pdfimages");
        fake_pdfimages_empty(&pdfimages);
        let pdftoppm = dir.path().join("pdftoppm");
        fake_pdftoppm(&pdftoppm);

        let tools = PopplerTools {
            pdftohtml,
            pdftotext,
            pdfimages: Some(pdfimages),
            pdftoppm: Some(pdftoppm),
        };
        let options = ConvertOptions {
            low_confidence: LowConfidenceMode::Preserve,
            ..ConvertOptions::default()
        };
        let outcome = convert_pdf_with_tools(&input, &output, &options, &tools)
            .expect("conversion should succeed");

        assert_eq!(outcome.report.low_confidence_pages, 1);
        assert_eq!(
            outcome.report.page_stats[0]
                .low_confidence_action
                .as_deref(),
            Some("preserve")
        );
        assert!(
            outcome
                .report
                .warnings
                .iter()
                .any(|warning| warning.contains("page 1: low-confidence")
                    && warning.contains("action=preserve"))
        );

        let mut archive =
            ZipArchive::new(fs::File::open(&output).expect("epub opens")).expect("zip opens");
        let mut content = String::new();
        archive
            .by_name("content.xhtml")
            .expect("content exists")
            .read_to_string(&mut content)
            .expect("content reads");
        assert!(content.contains("<figure id=\"pdf-page-0001\">"));
        assert!(content.contains("images/pdf-page-0001.png"));
        assert!(!content.contains("Tiny"));

        let mut image = Vec::new();
        archive
            .by_name("images/pdf-page-0001.png")
            .expect("preserved page image exists")
            .read_to_end(&mut image)
            .expect("image reads");
        assert!(image.starts_with(b"\x89PNG\r\n\x1a\n"));
    }
}
