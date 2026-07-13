use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MediaKind {
    Table,
    Equation,
}

impl MediaKind {
    pub(super) fn id_prefix(self) -> &'static str {
        match self {
            MediaKind::Table => "pdf-table",
            MediaKind::Equation => "pdf-equation",
        }
    }

    pub(super) fn crop_padding(self) -> i32 {
        match self {
            MediaKind::Table => 8,
            MediaKind::Equation => 10,
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            MediaKind::Table => "table",
            MediaKind::Equation => "equation",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RegionRect {
    pub(super) page: u32,
    pub(super) top: i32,
    pub(super) left: i32,
    pub(super) width: i32,
    pub(super) height: i32,
}

impl RegionRect {
    pub(super) fn right(self) -> i32 {
        self.left + self.width
    }

    pub(super) fn bottom(self) -> i32 {
        self.top + self.height
    }

    pub(super) fn padded(self, page: &Page, padding: i32) -> Self {
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

    pub(super) fn overlaps_fragment(self, fragment: &crate::model::Fragment) -> bool {
        fragment.top < self.bottom()
            && fragment.top + fragment.height > self.top
            && fragment.left < self.right()
            && fragment.right() > self.left
    }

    pub(super) fn area(self) -> i64 {
        i64::from(self.width.max(0)) * i64::from(self.height.max(0))
    }

    pub(super) fn overlap_area(self, other: Self) -> i64 {
        if self.page != other.page {
            return 0;
        }
        let left = self.left.max(other.left);
        let right = self.right().min(other.right());
        let top = self.top.max(other.top);
        let bottom = self.bottom().min(other.bottom());
        i64::from((right - left).max(0)) * i64::from((bottom - top).max(0))
    }

    pub(super) fn vertically_overlaps(self, other: Self) -> bool {
        self.page == other.page && self.top < other.bottom() && other.top < self.bottom()
    }

    pub(super) fn touches_within(self, other: Self, gap: i32) -> bool {
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
pub(super) struct MediaRegion {
    pub(super) kind: MediaKind,
    pub(super) rect: RegionRect,
    pub(super) caption: Option<Vec<Span>>,
}

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct MediaCounts {
    pub(super) tables: usize,
    pub(super) equations: usize,
}

pub(super) struct MediaFigures {
    pub(super) figures: Vec<AnchoredFigure>,
    pub(super) counts: MediaCounts,
    pub(super) warnings: Vec<String>,
}

pub(super) struct AnchoredFigure {
    pub(super) block: DocBlock,
    pub(super) anchor: BlockAnchor,
    pub(super) text_region: Option<RegionRect>,
}

pub(super) struct FigureBlocks {
    pub(super) figures: Vec<AnchoredFigure>,
    pub(super) warnings: Vec<String>,
    pub(super) exclusions: Vec<RegionRect>,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ImageFigureCandidate<'a> {
    pub(super) image: &'a ExtractedImage,
    pub(super) region: Option<&'a ImageRegion>,
    pub(super) caption: Option<&'a Fragment>,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct FigureCropRegion<'a> {
    pub(super) rect: RegionRect,
    pub(super) caption: &'a Fragment,
}

pub(super) fn media_figure_blocks(
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

pub(super) fn detect_media_regions(
    pages: &[Page],
    figure_exclusions: &[RegionRect],
) -> Vec<MediaRegion> {
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

pub(super) fn table_regions_for_page(page: &Page, excluded: &[RegionRect]) -> Vec<MediaRegion> {
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

pub(super) fn is_display_equation_fragment(page: &Page, fragment: &Fragment) -> bool {
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

pub(super) fn figure_blocks_from_images(
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

pub(super) fn image_candidate_clusters(
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

pub(super) fn image_figure_candidates<'a>(
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

pub(super) fn vector_figure_regions<'a>(
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

pub(super) fn padded_vector_crop_rect(page: &Page, rect: RegionRect) -> RegionRect {
    let mut crop = rect.padded(page, 48);
    if crop.top < rect.top {
        let bottom = crop.bottom();
        crop.top = rect.top;
        crop.height = (bottom - crop.top).max(1);
    }
    crop
}

pub(super) fn is_prose_like_fragment(fragment: &Fragment) -> bool {
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

pub(super) fn detect_caption_fragment<'a>(
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
