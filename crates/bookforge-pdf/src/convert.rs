//! End-to-end conversion orchestration: poppler → parse → reconstruct →
//! EPUB + report.

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    Result,
    epub::write_epub,
    model::{
        ColumnMode, DocBlock, Fragment, ImageAsset, ImageRegion, LowConfidenceMode, Page, Span,
    },
    parse::parse_pdf2xml,
    reconstruct::{BlockAnchor, PageStats, reconstruct},
    report::{ConversionReport, ReportMetrics},
    tools::{ExtractedImage, PageCrop, PopplerTools},
};

const LOW_CONFIDENCE_COVERAGE: f64 = 0.95;

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
    let media_dir = scoped_temp_dir("bookforge-pdf-media")?;
    let image_dir = scoped_temp_dir("bookforge-pdf-images")?;
    let extracted_images = tools.extract_images(input, &image_dir)?;
    let figure_result =
        figure_blocks_from_images(input, &pages, &extracted_images, tools, &media_dir)?;
    let mut figure_blocks = figure_result.figures;
    let media_figures = media_figure_blocks(input, &pages, tools, &media_dir);
    let _ = fs::remove_dir_all(&media_dir);
    let _ = fs::remove_dir_all(&image_dir);
    let media_figures = media_figures?;
    figure_blocks.extend(media_figures.figures);
    let mut blocks = reconstruction.blocks;
    let mut block_anchors = reconstruction.block_anchors;
    insert_figure_blocks(&mut blocks, &mut block_anchors, figure_blocks);
    let baseline = tools.pdf_to_text(input)?;
    let baseline_chars = baseline.chars().filter(|ch| !ch.is_whitespace()).count();
    let baseline_page_chars = baseline_page_char_counts(&baseline, reconstruction.pages.len());
    let mut page_stats = reconstruction.pages;
    for (stats, chars) in page_stats.iter_mut().zip(baseline_page_chars) {
        stats.baseline_chars = chars;
    }
    let low_confidence_pages = mark_low_confidence_pages(&mut page_stats, options.low_confidence);
    if options.low_confidence == LowConfidenceMode::Preserve {
        preserve_low_confidence_pages(
            input,
            &pages,
            tools,
            &mut blocks,
            &mut block_anchors,
            &low_confidence_pages,
        )?;
    }
    let mut layout_warnings = figure_result.warnings;
    layout_warnings.extend(media_layout_warnings(&blocks, &block_anchors));
    let reconstructed_chars: usize = blocks.iter().map(DocBlock::char_count).sum();
    let figure_count = blocks
        .iter()
        .filter(|block| matches!(block, DocBlock::Figure { .. }))
        .count();

    let title = if options.title.is_empty() {
        input
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Converted PDF".to_string())
    } else {
        options.title.clone()
    };
    write_epub(&blocks, &title, &options.language, output)?;

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
        && (stats.chars as f64 / stats.baseline_chars as f64) < LOW_CONFIDENCE_COVERAGE
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
    blocks: &mut Vec<DocBlock>,
    block_anchors: &mut Vec<BlockAnchor>,
    low_confidence_pages: &[u32],
) -> Result<()> {
    if low_confidence_pages.is_empty() {
        return Ok(());
    }

    let page_dir = scoped_temp_dir("bookforge-pdf-pages")?;
    let result = (|| {
        for page_number in low_confidence_pages {
            let rendered = tools.render_page_png(input, *page_number, &page_dir)?;
            let source_page = pages.iter().find(|page| page.number == *page_number);
            let asset = page_image_asset(*page_number, source_page, &rendered)?;
            replace_page_with_preserved_image(blocks, block_anchors, *page_number, asset);
        }
        Ok(())
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
    blocks: &mut Vec<DocBlock>,
    block_anchors: &mut Vec<BlockAnchor>,
    page_number: u32,
    image: ImageAsset,
) {
    let old_blocks = std::mem::take(blocks);
    let old_anchors = std::mem::take(block_anchors);
    let mut insert_at = None;

    for (block, anchor) in old_blocks.into_iter().zip(old_anchors) {
        if anchor.page == page_number {
            insert_at.get_or_insert(blocks.len());
            continue;
        }
        if anchor.page > page_number {
            insert_at.get_or_insert(blocks.len());
        }
        blocks.push(block);
        block_anchors.push(anchor);
    }

    let insert_at = insert_at.unwrap_or(blocks.len());
    blocks.insert(
        insert_at,
        DocBlock::Figure {
            image,
            caption: None,
        },
    );
    block_anchors.insert(
        insert_at,
        BlockAnchor {
            page: page_number,
            top: 0,
        },
    );
}

fn media_layout_warnings(blocks: &[DocBlock], block_anchors: &[BlockAnchor]) -> Vec<String> {
    let mut warnings = Vec::new();
    for (index, block) in blocks.iter().enumerate().skip(1) {
        if !matches!(blocks.get(index - 1), Some(DocBlock::Figure { .. }))
            || !starts_with_lowercase_or_suffix(block)
        {
            continue;
        }
        let Some(anchor) = block_anchors.get(index) else {
            continue;
        };
        warnings.push(format!(
            "page {}: lowercase paragraph continuation follows media block near y={}; review paragraph join",
            anchor.page, anchor.top
        ));
    }
    warnings
}

fn starts_with_lowercase_or_suffix(block: &DocBlock) -> bool {
    let DocBlock::Paragraph { spans } = block else {
        return false;
    };
    let text = fragment_text(spans);
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
}

struct AnchoredFigure {
    block: DocBlock,
    anchor: BlockAnchor,
    text_region: Option<RegionRect>,
}

struct FigureBlocks {
    figures: Vec<AnchoredFigure>,
    warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CaptionKey {
    page: u32,
    text: String,
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

fn media_figure_blocks(
    input: &Path,
    pages: &[Page],
    tools: &PopplerTools,
    output_dir: &Path,
) -> Result<MediaFigures> {
    let regions = detect_media_regions(pages);
    let mut figures = Vec::new();
    let mut counts = MediaCounts::default();

    for region in regions {
        let index = match region.kind {
            MediaKind::Table => {
                counts.tables += 1;
                counts.tables
            }
            MediaKind::Equation => {
                counts.equations += 1;
                counts.equations
            }
        };
        let name = format!("{}-{index:04}", region.kind.id_prefix());
        let rendered = tools.render_page_crop_png(
            input,
            PageCrop {
                page: region.rect.page,
                left: region.rect.left,
                top: region.rect.top,
                width: region.rect.width,
                height: region.rect.height,
            },
            output_dir,
            &name,
        )?;
        let asset = media_asset(region.kind, index, region.rect, &rendered)?;
        figures.push(AnchoredFigure {
            block: DocBlock::Figure {
                image: asset,
                caption: region.caption,
            },
            anchor: BlockAnchor {
                page: region.rect.page,
                top: region.rect.top,
            },
            text_region: Some(region.rect),
        });
    }

    Ok(MediaFigures { figures, counts })
}

fn detect_media_regions(pages: &[Page]) -> Vec<MediaRegion> {
    let mut regions = Vec::new();
    for page in pages {
        let mut page_regions = table_regions_for_page(page);
        let excluded = page_regions
            .iter()
            .map(|region| region.rect)
            .collect::<Vec<_>>();
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
        .filter(|fragment| fragment.width > 0 && !fragment_text(&fragment.spans).trim().is_empty())
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

fn table_regions_for_page(page: &Page) -> Vec<MediaRegion> {
    let rows = fragment_rows(page);
    let mut regions = Vec::new();
    let mut group: Vec<&FragmentRow<'_>> = Vec::new();

    for row in &rows {
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

fn push_table_region(page: &Page, rows: &[&FragmentRow<'_>], regions: &mut Vec<MediaRegion>) {
    if rows.len() < 3 || !table_group_has_aligned_columns(rows) {
        return;
    }
    let rect = rect_from_rows(page, rows).padded(page, 8);
    regions.push(MediaRegion {
        kind: MediaKind::Table,
        rect,
        caption: detect_table_caption(page, rect),
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
            let text = fragment_text(&fragment.spans);
            text.chars().any(|ch| ch.is_ascii_digit()) || text.contains('%')
        })
        .count();
    let short_cells = row
        .fragments
        .iter()
        .filter(|fragment| fragment_text(&fragment.spans).trim().chars().count() <= 32)
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
    let rect = rect_from_fragments(page, fragments).padded(page, 10);
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
    let text = fragment_text(&fragment.spans);
    let trimmed = text.trim();
    if trimmed.chars().count() < 3 || is_caption_text(trimmed) {
        return false;
    }
    let nonspace = trimmed.chars().filter(|ch| !ch.is_whitespace()).count();
    let math_symbols = trimmed.chars().filter(|ch| is_math_symbol(*ch)).count();
    let word_count = trimmed.split_whitespace().count();
    let centered = ((fragment.left + fragment.width / 2) - page.width / 2).abs() <= page.width / 5;
    let short = fragment.width <= page.width * 7 / 10;
    let has_strong_operator = trimmed.contains('=')
        || trimmed.chars().any(|ch| {
            matches!(
                ch,
                '\u{2211}'
                    | '\u{222b}'
                    | '\u{221a}'
                    | '\u{2264}'
                    | '\u{2265}'
                    | '\u{2248}'
                    | '\u{2260}'
            )
        });

    centered
        && short
        && has_strong_operator
        && word_count <= 8
        && math_symbols >= 2
        && math_symbols * 3 >= nonspace
}

fn is_math_symbol(ch: char) -> bool {
    matches!(
        ch,
        '=' | '+'
            | '-'
            | '*'
            | '/'
            | '^'
            | '_'
            | '('
            | ')'
            | '['
            | ']'
            | '{'
            | '}'
            | '|'
            | '<'
            | '>'
            | '\u{2211}'
            | '\u{222b}'
            | '\u{221a}'
            | '\u{2264}'
            | '\u{2265}'
            | '\u{2248}'
            | '\u{2260}'
            | '\u{00b1}'
            | '\u{00d7}'
            | '\u{00f7}'
            | '\u{2202}'
            | '\u{2207}'
            | '\u{221e}'
            | '\u{2208}'
    )
}

fn figure_blocks_from_images(
    input: &Path,
    pages: &[Page],
    images: &[ExtractedImage],
    tools: &PopplerTools,
    output_dir: &Path,
) -> Result<FigureBlocks> {
    let pages_by_number = pages
        .iter()
        .map(|page| (page.number, page))
        .collect::<HashMap<_, _>>();
    let candidates = image_figure_candidates(&pages_by_number, images);
    let handled_captions = candidates
        .iter()
        .filter_map(|candidate| Some(caption_key(candidate.image.page, &candidate.caption?.spans)))
        .collect::<HashSet<_>>();
    let mut used_images = vec![false; candidates.len()];
    let mut figures = Vec::new();
    let mut warnings = Vec::new();
    let mut figure_crop_count = 0;

    for (index, candidate) in candidates.iter().enumerate() {
        if used_images[index] {
            continue;
        }
        let (Some(_region), Some(caption)) = (candidate.region, candidate.caption) else {
            continue;
        };
        let key = caption_key(candidate.image.page, &caption.spans);
        let group = candidates
            .iter()
            .enumerate()
            .filter(|(group_index, group_candidate)| {
                !used_images[*group_index]
                    && group_candidate.region.is_some()
                    && group_candidate.caption.is_some_and(|group_caption| {
                        caption_key(group_candidate.image.page, &group_caption.spans) == key
                    })
            })
            .collect::<Vec<_>>();
        if group.len() < 2 {
            continue;
        }

        let Some(page) = pages_by_number.get(&candidate.image.page).copied() else {
            continue;
        };
        let regions = group
            .iter()
            .filter_map(|(_, candidate)| candidate.region)
            .collect::<Vec<_>>();
        let rect = rect_from_image_regions(page, &regions).padded(page, 8);
        let (rect, snapped) = snap_rect_above_caption(rect, caption);
        figure_crop_count += 1;
        let asset = render_figure_crop(
            input,
            tools,
            output_dir,
            figure_crop_count,
            rect,
            "pdf-figure",
        )?;
        if snapped {
            warnings.push(caption_snap_warning(rect.page, caption.top));
        }
        figures.push(AnchoredFigure {
            block: DocBlock::Figure {
                image: asset,
                caption: Some(caption.spans.clone()),
            },
            anchor: BlockAnchor {
                page: rect.page,
                top: rect.top,
            },
            text_region: Some(rect),
        });
        for (group_index, _) in group {
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
        let top = candidate
            .region
            .map(|region| region.top)
            .unwrap_or(i32::MAX);
        let asset = image_asset(candidate.image, candidate.region)?;
        figures.push(AnchoredFigure {
            block: DocBlock::Figure {
                image: asset,
                caption: candidate.caption.map(|caption| caption.spans.clone()),
            },
            anchor: BlockAnchor {
                page: candidate.image.page,
                top,
            },
            text_region: None,
        });
    }

    for region in vector_figure_regions(pages, &handled_captions) {
        figure_crop_count += 1;
        let rect = region.rect.padded(
            pages_by_number
                .get(&region.rect.page)
                .copied()
                .expect("vector figure regions come from known pages"),
            48,
        );
        let (rect, snapped) = snap_rect_above_caption(rect, region.caption);
        let asset = render_figure_crop(
            input,
            tools,
            output_dir,
            figure_crop_count,
            rect,
            "pdf-figure",
        )?;
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
            },
            text_region: Some(rect),
        });
    }

    Ok(FigureBlocks { figures, warnings })
}

fn image_figure_candidates<'a>(
    pages_by_number: &HashMap<u32, &'a Page>,
    images: &'a [ExtractedImage],
) -> Vec<ImageFigureCandidate<'a>> {
    let mut page_image_counts: HashMap<u32, usize> = HashMap::new();
    let mut candidates = Vec::new();

    for image in images {
        let page = pages_by_number.get(&image.page).copied();
        let page_index = page_image_counts.entry(image.page).or_default();
        let region = page.and_then(|page| page.images.get(*page_index));
        *page_index += 1;

        candidates.push(ImageFigureCandidate {
            image,
            region,
            caption: page.and_then(|page| detect_caption_fragment(page, region)),
        });
    }

    candidates
}

fn vector_figure_regions<'a>(
    pages: &'a [Page],
    handled_captions: &HashSet<CaptionKey>,
) -> Vec<FigureCropRegion<'a>> {
    let mut regions = Vec::new();
    for page in pages {
        for caption in figure_caption_fragments(page) {
            let key = caption_key(page.number, &caption.spans);
            if handled_captions.contains(&key) {
                continue;
            }
            let chart_fragments = chart_label_fragments(page, caption);
            if chart_fragments.len() < 4
                || chart_fragments
                    .iter()
                    .filter(|fragment| {
                        fragment_text(&fragment.spans)
                            .chars()
                            .any(|ch| ch.is_ascii_digit())
                    })
                    .count()
                    < 3
            {
                continue;
            }
            regions.push(FigureCropRegion {
                rect: rect_from_fragments(page, &chart_fragments),
                caption,
            });
        }
    }
    regions
}

fn chart_label_fragments<'a>(page: &'a Page, caption: &Fragment) -> Vec<&'a Fragment> {
    page.fragments
        .iter()
        .filter(|fragment| {
            let bottom = fragment.top + fragment.height;
            bottom <= caption.top
                && caption.top.saturating_sub(bottom) <= 360
                && is_chart_label_fragment(fragment)
        })
        .collect()
}

fn is_chart_label_fragment(fragment: &Fragment) -> bool {
    let text = fragment_text(&fragment.spans);
    let trimmed = text.trim();
    if trimmed.is_empty() || is_caption_text(trimmed) {
        return false;
    }
    let chars = trimmed.chars().count();
    let words = trimmed.split_whitespace().count();
    let has_digit = trimmed.chars().any(|ch| ch.is_ascii_digit());
    let short_axis_label = words <= 3 && chars <= 24;
    (has_digit || short_axis_label) && words <= 4 && chars <= 28
}

fn rect_from_image_regions(page: &Page, regions: &[&ImageRegion]) -> RegionRect {
    let left = regions.iter().map(|region| region.left).min().unwrap_or(0);
    let right = regions
        .iter()
        .map(|region| region.left + region.width)
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
    input: &Path,
    tools: &PopplerTools,
    output_dir: &Path,
    index: usize,
    rect: RegionRect,
    prefix: &str,
) -> Result<ImageAsset> {
    let name = format!("{prefix}-{index:04}");
    let rendered = tools.render_page_crop_png(
        input,
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

fn caption_key(page: u32, spans: &[Span]) -> CaptionKey {
    CaptionKey {
        page,
        text: normalize_caption(&fragment_text(spans)),
    }
}

fn figure_caption_fragments(page: &Page) -> Vec<&Fragment> {
    page.fragments
        .iter()
        .filter(|fragment| is_figure_caption_text(&fragment_text(&fragment.spans)))
        .collect()
}

fn detect_caption_fragment<'a>(
    page: &'a Page,
    region: Option<&ImageRegion>,
) -> Option<&'a Fragment> {
    let mut candidates = figure_caption_fragments(page);
    candidates.sort_by_key(|fragment| fragment.top);

    if let Some(region) = region {
        let bottom = region.bottom();
        candidates
            .into_iter()
            .filter(|fragment| fragment.top >= bottom.saturating_sub(8))
            .min_by_key(|fragment| fragment.top.saturating_sub(bottom))
            .filter(|fragment| fragment.top.saturating_sub(bottom) <= 160)
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
        .filter(|fragment| is_table_caption_text(&fragment_text(&fragment.spans)))
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
    blocks: &mut Vec<DocBlock>,
    block_anchors: &mut Vec<BlockAnchor>,
    mut figures: Vec<AnchoredFigure>,
) -> usize {
    figures.sort_by_key(|figure| (figure.anchor.page, figure.anchor.top));
    let count = figures.len();

    for figure in figures {
        if let Some(region) = figure.text_region {
            remove_blocks_in_region(blocks, block_anchors, region);
        }
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

fn remove_blocks_in_region(
    blocks: &mut Vec<DocBlock>,
    block_anchors: &mut Vec<BlockAnchor>,
    region: RegionRect,
) {
    let mut index = 0;
    while index < block_anchors.len() {
        let anchor = block_anchors[index];
        if anchor.page == region.page && anchor.top >= region.top && anchor.top <= region.bottom() {
            blocks.remove(index);
            block_anchors.remove(index);
        } else {
            index += 1;
        }
    }
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

    const BERT_FIGURE1_CAPTION_BOUNDARY_XML: &str =
        include_str!("../fixtures/bert_figure1_caption_boundary.xml");
    const BERT_FIGURE4_MULTIPANEL_XML: &str =
        include_str!("../fixtures/bert_figure4_multipanel.xml");
    const BERT_FIGURE5_VECTOR_CHART_XML: &str =
        include_str!("../fixtures/bert_figure5_vector_chart.xml");
    const BERT_MODEL_PARAMETER_FALSE_POSITIVE_XML: &str =
        include_str!("../fixtures/bert_model_parameter_false_positive.xml");

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
    fn fake_pdftoppm_record_args(path: &Path) {
        write_executable(
            path,
            r#"#!/bin/sh
for last do :; done
printf '%s\n' "$*" > "$last.png"
"#,
        );
    }

    #[cfg(unix)]
    fn fake_pdftoppm(path: &Path) {
        write_executable(
            path,
            r#"#!/bin/sh
for last do :; done
printf 'fake-page' > "$last.png"
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
        let pdftoppm = dir.path().join("pdftoppm");
        fake_pdftoppm(&pdftoppm);

        let tools = PopplerTools {
            pdftohtml,
            pdftotext,
            pdfimages,
            pdftoppm,
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
        let pdftoppm = dir.path().join("pdftoppm");
        fake_pdftoppm(&pdftoppm);

        let tools = PopplerTools {
            pdftohtml,
            pdftotext,
            pdfimages,
            pdftoppm,
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
        fake_pdftoppm_record_args(&pdftoppm);

        let tools = PopplerTools {
            pdftohtml,
            pdftotext,
            pdfimages,
            pdftoppm,
        };
        let outcome = convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), &tools)
            .expect("conversion should succeed");

        assert_eq!(outcome.report.images, 2);
        assert_eq!(outcome.report.figures, 1);
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

        let mut image = String::new();
        archive
            .by_name("images/pdf-figure-0001.png")
            .expect("figure crop exists")
            .read_to_string(&mut image)
            .expect("crop args read");
        assert!(
            image.contains("-H 100"),
            "crop should stop above the caption baseline; args were {image:?}"
        );
    }

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
            pdfimages,
            pdftoppm,
        };
        let outcome = convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), &tools)
            .expect("conversion should succeed");

        assert_eq!(outcome.report.images, 2);
        assert_eq!(outcome.report.figures, 1);

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
            pdfimages,
            pdftoppm,
        };
        let outcome = convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), &tools)
            .expect("conversion should succeed");

        assert_eq!(outcome.report.images, 0);
        assert_eq!(outcome.report.figures, 1);
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
            pdfimages,
            pdftoppm,
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
            pdfimages,
            pdftoppm,
        };
        let outcome = convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), &tools)
            .expect("conversion should succeed");

        assert_eq!(outcome.report.tables, 1);
        assert_eq!(outcome.report.equations, 0);
        assert_eq!(outcome.report.figures, 1);

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
        assert_eq!(image, b"fake-page");
    }

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
            pdfimages,
            pdftoppm,
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
        assert_eq!(image, b"fake-page");
    }

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
            pdfimages,
            pdftoppm,
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
            pdfimages,
            pdftoppm,
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
            pdfimages,
            pdftoppm,
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
            pdfimages,
            pdftoppm,
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
        assert_eq!(image, b"fake-page");
    }
}
