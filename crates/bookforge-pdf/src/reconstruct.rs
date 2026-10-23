//! Deterministic layout reconstruction: fragments → lines → columns →
//! reading order → paragraphs/headings.
//!
//! The heuristics here are deliberately simple and inspectable. They are
//! tuned for the two layouts that matter first (ROADMAP §9b.1):
//! single-column books and two-column scientific papers. Anything the
//! heuristics get wrong shows up in the conversion report as a per-page
//! coverage gap, never as silently dropped text.

use std::collections::{HashMap, HashSet};

use crate::bidi;
use crate::model::{
    ColumnMode, DocBlock, Fragment, Line, Page, Span, count_visible_chars, normalize_text_key,
    spans_text,
};

/// Per-page reconstruction diagnostics for the conversion report.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PageStats {
    pub page: u32,
    pub lines: usize,
    pub chars: usize,
    pub baseline_chars: usize,
    /// Non-whitespace characters removed as repeated running
    /// headers/footers and page-number folios. The low-confidence coverage
    /// threshold is judged against `chars + running_header_chars` (the
    /// pre-removal view) so legitimate pages with recurring headers are not
    /// rasterized into OCR just because the header text dominates the
    /// deficit (docs/report.md §4.5 PDF-6).
    #[serde(default)]
    pub running_header_chars: usize,
    pub two_column: bool,
    /// The page's strong-script evidence leans right-to-left (PDF-7).
    /// Reported so consumers can attribute extraction quirks to script
    /// rather than conversion quality; it never gates coverage itself.
    #[serde(default)]
    pub rtl_dominant: bool,
    pub low_confidence: bool,
    pub low_confidence_action: Option<String>,
}

pub struct Reconstruction {
    pub blocks: Vec<AnchoredBlock>,
    pub pages: Vec<PageStats>,
    /// Rotated/zero-width fragments excluded from the reading flow,
    /// reported (not silenced) per docs/report.md §4.5.
    pub rotated_dropped_fragments: usize,
    /// Fragments lying wholly outside their page box, excluded from the
    /// reading flow. A title set across a two-page spread is reported on
    /// both pages, its other half shifted past the page edge.
    pub offpage_dropped_fragments: usize,
    /// Text read at the head of a page that continued a paragraph
    /// anchored on an earlier page.
    pub carried: Vec<CarriedText>,
}

/// The opening text of `page` joined onto a paragraph anchored on
/// `anchor_page`. A block's characters all count toward its anchor, so
/// per-page accounting moves this share back to the page it was read on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CarriedText {
    pub page: u32,
    pub anchor_page: u32,
    pub chars: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockAnchor {
    pub page: u32,
    pub top: i32,
    pub left: i32,
    pub width: i32,
}

#[derive(Debug, Clone)]
pub struct AnchoredBlock {
    pub block: DocBlock,
    pub anchor: BlockAnchor,
}

#[derive(Debug, Clone)]
struct PendingBlock {
    block: DocBlock,
    top: i32,
    left: i32,
    right: i32,
    /// Type size of the block's first line.
    font_size: u32,
}

pub fn reconstruct(pages: &[Page], columns: ColumnMode) -> Reconstruction {
    reconstruct_with_chapter_guard(pages, columns, None)
}

/// [`reconstruct`] with the chapter-prefix guard active (PDF-9): the
/// cross-page paragraph merge must never join a paragraph into a block
/// that itself opens a new chapter group, because chapter splitting
/// (epub.rs) matches block text against this same prefix and would
/// otherwise swallow the boundary line into the previous chapter.
pub fn reconstruct_with_chapter_guard(
    pages: &[Page],
    columns: ColumnMode,
    chapter_prefix: Option<&str>,
) -> Reconstruction {
    let body_size = body_font_size(pages);
    let heading_levels = heading_levels(pages, body_size);
    let running_margin_texts = running_margin_texts(pages, body_size);
    let folios = folio_lines(pages, body_size);
    let page_count = pages.iter().map(|page| page.number).max().unwrap_or(0);

    let mut blocks: Vec<AnchoredBlock> = Vec::new();
    // Type size of each block in `blocks`, kept in step with it.
    let mut block_sizes: Vec<u32> = Vec::new();
    let mut stats = Vec::new();
    let mut rotated_dropped_fragments = 0usize;
    let mut offpage_dropped_fragments = 0usize;
    let mut carried = Vec::new();

    for page in pages {
        rotated_dropped_fragments += rotated_fragment_count(page);
        offpage_dropped_fragments += page
            .fragments
            .iter()
            .filter(|fragment| fragment.width > 0 && !is_on_page(page, fragment))
            .count();
        let lines = merge_fragments_into_lines(page);
        let two_column = match columns {
            ColumnMode::Single => false,
            ColumnMode::Two => true,
            ColumnMode::Auto => detect_two_columns(page, &lines),
        };
        let mut ordered = if two_column {
            order_two_column(page, &lines)
        } else {
            let mut ordered = lines.clone();
            ordered.sort_by_key(|line| (line.top, line.left));
            ordered
        };
        let contents_numbers = contents_page_numbers(&lines, page_count);
        let page_lines = lines_by_position(page);
        let page_median_gap = median_line_gap(&page_lines);
        let mut running_header_chars = 0usize;
        ordered.retain(|line| {
            if is_running_margin_line(
                page,
                &page_lines,
                page_median_gap,
                line,
                body_size,
                &running_margin_texts,
            ) || folios.contains(&(page.number, line.top, line.left))
                || contents_numbers.contains(&(line.top, line.left))
            {
                running_header_chars += line.char_count();
                return false;
            }
            true
        });

        // PDF-7: poppler emits each RTL fragment's characters in logical
        // order but reconstruction concatenates fragments visually left
        // to right, so dominant-RTL lines read backwards. Repair runs
        // per line; LTR-dominant lines pass through untouched.
        for line in &mut ordered {
            if let Some(spans) = bidi::reorder_line_spans(&line.spans) {
                line.spans = spans;
            }
        }
        let rtl_dominant = page_is_rtl_dominant(&ordered);

        stats.push(PageStats {
            page: page.number,
            lines: ordered.len(),
            chars: ordered.iter().map(Line::char_count).sum(),
            baseline_chars: 0,
            running_header_chars,
            two_column,
            rtl_dominant,
            low_confidence: false,
            low_confidence_action: None,
        });

        let page_blocks = cluster_paragraphs(&ordered, body_size, &heading_levels);
        append_with_continuation(
            &mut blocks,
            &mut block_sizes,
            &mut carried,
            page.number,
            page_blocks,
            chapter_prefix,
        );
    }

    Reconstruction {
        blocks,
        pages: stats,
        rotated_dropped_fragments,
        offpage_dropped_fragments,
        carried,
    }
}

/// A page votes RTL when its strong-letter evidence leans Arabic/
/// Hebrew-class and carries enough volume to mean it (PDF-7).
fn page_is_rtl_dominant(lines: &[Line]) -> bool {
    let mut rtl = 0usize;
    let mut other = 0usize;
    for line in lines {
        let (line_rtl, line_other) = bidi::rtl_letter_counts(&line.text());
        rtl += line_rtl;
        other += line_other;
    }
    rtl > other && rtl >= 4
}

/// Rotated text (watermarks, vertical labels) is reported with
/// zero width by poppler and excluded from line merging; count it so
/// the conversion report can say what was skipped instead of dropping
/// it silently.
fn rotated_fragment_count(page: &Page) -> usize {
    page.fragments
        .iter()
        .filter(|fragment| fragment.width <= 0 && !spans_text(&fragment.spans).trim().is_empty())
        .count()
}

/// Whether any of the fragment lies inside the page box. A page without a
/// declared width keeps everything.
fn is_on_page(page: &Page, fragment: &Fragment) -> bool {
    page.width <= 0 || (fragment.left < page.width && fragment.right() > 0)
}

fn running_margin_texts(pages: &[Page], body_size: u32) -> HashSet<String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for page in pages {
        let mut seen_on_page = HashSet::new();
        let lines = lines_by_position(page);
        let median_gap = median_line_gap(&lines);
        for line in &lines {
            if line.font_size > body_size + 1
                || !near_vertical_margin(page, line)
                || !stands_apart(page, &lines, line, median_gap)
            {
                continue;
            }
            let normalized = normalize_text_key(&line.text());
            if normalized.len() >= 4 && normalized.chars().any(|ch| ch.is_alphabetic()) {
                seen_on_page.insert(normalized);
            }
        }
        for text in seen_on_page {
            *counts.entry(text).or_default() += 1;
        }
    }
    // Only treat a near-margin line as a running header/footer when the same
    // text repeats near the margins on at least two pages. Removing a one-off
    // line just because it is bold or larger (heading-like) silently deleted
    // legitimate body text that merely sat near a page margin.
    counts
        .into_iter()
        .filter_map(|(text, count)| (count >= 2).then_some(text))
        .collect()
}

fn is_running_margin_line(
    page: &Page,
    page_lines: &[Line],
    median_gap: i32,
    line: &Line,
    body_size: u32,
    running_margin_texts: &HashSet<String>,
) -> bool {
    line.font_size <= body_size + 1
        && near_vertical_margin(page, line)
        && running_margin_texts.contains(&normalize_text_key(&line.text()))
        && stands_apart(page, page_lines, line, median_gap)
}

/// The page's lines in reading position, top to bottom.
fn lines_by_position(page: &Page) -> Vec<Line> {
    let mut lines = merge_fragments_into_lines(page);
    lines.sort_by_key(|line| (line.top, line.left));
    lines
}

/// Whether a margin line stands apart from the text beside it, as running
/// heads and feet do: the nearest line on the text-block side is farther
/// away than one and a half line heights or the page's usual line spacing.
/// A footnote line that happens to repeat at the foot of two pages
/// ("Title, 189.") sits at ordinary leading under the note line above it,
/// and was deleted as a running footer.
fn stands_apart(page: &Page, lines: &[Line], line: &Line, median_gap: i32) -> bool {
    let below_middle = line.top > page.height / 2;
    let same_baseline = line.height.max(1) / 2;
    lines
        .iter()
        // Only text set at about the line's own size is the flow it might
        // belong to; a chapter title under a running head is not.
        .filter(|other| other.font_size.abs_diff(line.font_size) <= 2)
        .map(|other| {
            if below_middle {
                line.top - other.top
            } else {
                other.top - line.top
            }
        })
        .filter(|gap| *gap > same_baseline)
        .min()
        .is_none_or(|gap| gap * 10 > line.height.max(1) * 15 || gap * 10 > median_gap.max(1) * 13)
}

/// Printed page references on a contents page, keyed by `(top, left)`. An
/// entry sets its page number flush right on its first line, too far from
/// the title to join that line, so each number became a paragraph of its own
/// or ran into the next entry. Page numbers mean nothing in a reflowable
/// book, so they are dropped. A number counts only on a page holding at
/// least three that share a right edge, each the last text on a line that
/// starts to its left, rising down the page (roman before arabic) and none
/// above the document's page count, which keeps years and table figures out.
fn contents_page_numbers(lines: &[Line], page_count: u32) -> HashSet<(i32, i32)> {
    let mut candidates: Vec<(&Line, bool, i64)> = lines
        .iter()
        .filter_map(|line| {
            let (roman, value) = folio_value(&line.text())?;
            if !roman && value > i64::from(page_count) {
                return None;
            }
            let tolerance = line.height.max(1) / 2;
            let mut on_baseline = lines
                .iter()
                .filter(|other| !std::ptr::eq(*other, line))
                .filter(|other| (other.top - line.top).abs() <= tolerance);
            let entry_to_the_left = on_baseline
                .clone()
                .any(|other| other.right + line.height <= line.left);
            let last_on_line = !on_baseline.any(|other| other.left > line.left);
            (entry_to_the_left && last_on_line).then_some((line, roman, value))
        })
        .collect();
    let Some(right_edge) = candidates.iter().map(|(line, ..)| line.right).max() else {
        return HashSet::new();
    };
    candidates.retain(|(line, ..)| right_edge - line.right <= line.height);
    candidates.sort_by_key(|(line, ..)| line.top);
    let mut previous: Option<(bool, i64)> = None;
    let rising = candidates.iter().all(|(_, roman, value)| {
        let ok = match previous {
            None => true,
            Some((true, before)) => !*roman || *value > before,
            Some((false, before)) => !*roman && *value > before,
        };
        previous = Some((*roman, *value));
        ok
    });
    if candidates.len() < 3 || !rising {
        return HashSet::new();
    }
    candidates
        .iter()
        .map(|(line, ..)| (line.top, line.left))
        .collect()
}

/// How many pages apart two margin numbers may be and still corroborate each
/// other as folios (recto-only numbering skips every other page).
const FOLIO_NEIGHBOUR_PAGES: u32 = 3;

/// Page numbers printed in the top or bottom margin, keyed by
/// `(page, top, left)`. A folio's text changes on every page, so
/// [`running_margin_texts`] never matches it, and left in place it becomes a
/// paragraph of its own that also blocks the cross-page merge, splitting
/// every sentence that runs over a page break. A bare number in the margin
/// band counts as a folio only when another page within
/// [`FOLIO_NEIGHBOUR_PAGES`] carries one whose value differs by exactly the
/// page distance; an isolated margin number is kept as text.
fn folio_lines(pages: &[Page], body_size: u32) -> HashSet<(u32, i32, i32)> {
    let mut candidates = Vec::new();
    for page in pages {
        for line in merge_fragments_into_lines(page) {
            // Folios are often set a little larger or bolder than the body;
            // the window still excludes superscript note markers and large
            // display numerals such as chapter numbers.
            let size = line.font_size * 10;
            if size < body_size * 7 || size > body_size * 15 || !near_vertical_margin(page, &line) {
                continue;
            }
            if let Some((roman, value)) = folio_value(&line.text()) {
                candidates.push((page.number, roman, value, line.top, line.left));
            }
        }
    }
    candidates
        .iter()
        .filter(|(page, roman, value, ..)| {
            candidates
                .iter()
                .any(|(other_page, other_roman, other_value, ..)| {
                    other_page != page
                        && other_roman == roman
                        && other_page.abs_diff(*page) <= FOLIO_NEIGHBOUR_PAGES
                        && other_value - value == i64::from(*other_page) - i64::from(*page)
                })
        })
        .map(|(page, _, _, top, left)| (*page, *top, *left))
        .collect()
}

/// Value of a bare folio: up to four ASCII digits, or a single-case roman
/// numeral as used in front matter. `true` marks roman values, which only
/// corroborate other roman folios.
fn folio_value(text: &str) -> Option<(bool, i64)> {
    let text = text.trim();
    if !text.is_empty() && text.len() <= 4 && text.bytes().all(|byte| byte.is_ascii_digit()) {
        return text.parse().ok().map(|value| (false, value));
    }
    if text.is_empty()
        || text.len() > 8
        || (text != text.to_ascii_lowercase() && text != text.to_ascii_uppercase())
    {
        return None;
    }
    let digits = text
        .to_ascii_lowercase()
        .chars()
        .map(|ch| match ch {
            'i' => Some(1),
            'v' => Some(5),
            'x' => Some(10),
            'l' => Some(50),
            'c' => Some(100),
            _ => None,
        })
        .collect::<Option<Vec<i64>>>()?;
    let value = digits.iter().enumerate().fold(0, |total, (index, digit)| {
        if digits.get(index + 1).is_some_and(|next| next > digit) {
            total - digit
        } else {
            total + digit
        }
    });
    (value > 0).then_some((true, value))
}

fn near_vertical_margin(page: &Page, line: &Line) -> bool {
    let margin = (page.height / 8).max(line.height * 3);
    line.top <= margin || line.top + line.height >= page.height - margin
}

/// Most visible characters a note reference mark can have ("123", "†").
const RAISED_MARK_MAX_CHARS: usize = 4;

/// A visual line under construction: its geometry plus member fragments.
struct LineGroup<'a> {
    line: Line,
    members: Vec<&'a Fragment>,
}

/// Group fragments that share a baseline into one visual line, joining
/// fragment gaps with a space when they are visually separated. A note
/// reference mark (short, much smaller, raised) joins the line it sits on
/// at its horizontal position as a superscript, even mid-line: pdftohtml
/// reports such a mark as its own line, which used to become a paragraph of
/// its own and cut the sentence around it in two.
fn merge_fragments_into_lines(page: &Page) -> Vec<Line> {
    // poppler reports rotated text (arXiv margin watermarks, sidebar
    // decorations) with zero width. It is not part of the reading flow
    // and, left in, it skews the column-midline estimate. Text wholly off
    // the page is the other half of a spread and is read on its own page.
    let mut fragments: Vec<&Fragment> = page
        .fragments
        .iter()
        .filter(|fragment| fragment.width > 0 && is_on_page(page, fragment))
        .collect();
    fragments.sort_by_key(|fragment| (fragment.top, fragment.left));

    let mut groups: Vec<LineGroup> = Vec::new();
    for fragment in fragments {
        let size = fragment_font_size(page, fragment);
        let same_line = groups.last().is_some_and(|group| {
            let line = &group.line;
            let tolerance = (line.height.max(fragment.height) as f32 * 0.5) as i32;
            // The gap cap keeps same-baseline lines in *different columns*
            // from merging. Style-split fragments sit flush and justified
            // word gaps stay under ~1em, while column gutters run wider
            // than the line height — observed as low as ~1.7× height on
            // two-column papers, so the cap must stay below that.
            let max_join_gap = line.height.max(fragment.height) * 5 / 4;
            (fragment.top - line.top).abs() <= tolerance
                && fragment.left >= line.right - 2
                && fragment.left - line.right <= max_join_gap
        });

        if same_line {
            let group = groups.last_mut().expect("checked above");
            group.line.right = group.line.right.max(fragment.right());
            group.line.height = group.line.height.max(fragment.height);
            group.line.font_size = group.line.font_size.max(size);
            group.members.push(fragment);
        } else {
            groups.push(LineGroup {
                line: Line {
                    top: fragment.top,
                    left: fragment.left,
                    right: fragment.right(),
                    height: fragment.height,
                    font_size: size,
                    spans: Vec::new(),
                },
                members: vec![fragment],
            });
        }
    }

    attach_raised_marks(page, &mut groups);

    let mut lines: Vec<Line> = groups
        .into_iter()
        .filter(|group| !group.members.is_empty())
        .map(|group| build_line(page, group))
        .collect();
    lines.retain(|line| !line.text().trim().is_empty());
    lines
}

fn fragment_font_size(page: &Page, fragment: &Fragment) -> u32 {
    page.font_sizes
        .get(&fragment.font)
        .copied()
        .unwrap_or(fragment.height.unsigned_abs())
}

/// A short fragment at most 70% of `host`'s size whose bottom sits above the
/// lower quarter of `host`: a superscript mark, not a subscript, small
/// capitals or a same-baseline word.
fn is_raised_mark(page: &Page, fragment: &Fragment, host: &Line) -> bool {
    let visible = fragment
        .spans
        .iter()
        .flat_map(|span| span.text.chars())
        .filter(|ch| !ch.is_whitespace())
        .count();
    (1..=RAISED_MARK_MAX_CHARS).contains(&visible)
        && fragment_font_size(page, fragment) * 10 <= host.font_size * 7
        && fragment.top + fragment.height > host.top
        && fragment.top < host.top + host.height
        && fragment.top + fragment.height <= host.top + host.height * 3 / 4
}

/// Move each single-fragment group that is a raised mark into the nearest
/// line it overlaps horizontally, leaving its own group empty.
fn attach_raised_marks(page: &Page, groups: &mut [LineGroup]) {
    for mark_index in 0..groups.len() {
        let [fragment] = groups[mark_index].members[..] else {
            continue;
        };
        let host_index = groups
            .iter()
            .enumerate()
            .filter(|(index, group)| {
                let host = &group.line;
                *index != mark_index
                    && !group.members.is_empty()
                    && fragment.left >= host.left - host.height
                    && fragment.left <= host.right + host.height * 5 / 4
                    && is_raised_mark(page, fragment, host)
            })
            .min_by_key(|(_, group)| (group.line.top - fragment.top).abs())
            .map(|(index, _)| index);
        if let Some(host_index) = host_index {
            groups[mark_index].members.clear();
            let host = &mut groups[host_index];
            host.line.right = host.line.right.max(fragment.right());
            host.members.push(fragment);
        }
    }
}

/// Spans of `group`'s members in left-to-right order, with a space at each
/// visible gap. Raised marks attach to the preceding text with no space.
fn build_line(page: &Page, mut group: LineGroup) -> Line {
    group.members.sort_by_key(|fragment| fragment.left);
    let mut line = group.line;
    let mut right: Option<i32> = None;
    for fragment in group.members {
        let raised = right.is_some() && is_raised_mark(page, fragment, &line);
        if raised {
            if let Some(last) = line.spans.last_mut() {
                let trimmed = last.text.trim_end().len();
                last.text.truncate(trimmed);
            }
        } else if right.is_some_and(|right| fragment.left - right > 1)
            && !line
                .spans
                .last()
                .is_some_and(|span| span.text.ends_with(' '))
        {
            push_joined(&mut line.spans, " ", false, false);
        }
        for span in &fragment.spans {
            push_styled(
                &mut line.spans,
                Span {
                    superscript: raised || span.superscript,
                    ..span.clone()
                },
            );
        }
        right = Some(right.map_or(fragment.right(), |right| right.max(fragment.right())));
    }
    line
}

fn push_joined(spans: &mut Vec<Span>, text: &str, bold: bool, italic: bool) {
    push_styled(
        spans,
        Span {
            text: text.to_string(),
            bold,
            italic,
            superscript: false,
        },
    );
}

/// Append `span`, extending the last span when its whole style matches.
fn push_styled(spans: &mut Vec<Span>, span: Span) {
    if span.text.is_empty() {
        return;
    }
    if let Some(last) = spans.last_mut()
        && last.bold == span.bold
        && last.italic == span.italic
        && last.superscript == span.superscript
    {
        last.text.push_str(&span.text);
        return;
    }
    spans.push(span);
}

/// A page is two-column when most non-full-width lines sit entirely in
/// the left or right half with a clear gutter and few lines crossing it.
fn detect_two_columns(page: &Page, lines: &[Line]) -> bool {
    if lines.len() < 8 {
        return false;
    }
    let content_left = lines.iter().map(|line| line.left).min().unwrap_or(0);
    let content_right = lines
        .iter()
        .map(|line| line.right)
        .max()
        .unwrap_or(page.width);
    let content_width = (content_right - content_left).max(1);
    let mid = content_left + content_width / 2;
    let slack = content_width / 20;

    let column_lines: Vec<&Line> = lines
        .iter()
        .filter(|line| line.width() < (content_width as f32 * 0.62) as i32)
        .collect();
    if column_lines.len() < 6 {
        return false;
    }

    let left = column_lines
        .iter()
        .filter(|line| line.right <= mid + slack)
        .count();
    let right = column_lines
        .iter()
        .filter(|line| line.left >= mid - slack)
        .count();
    let crossing = column_lines.len().saturating_sub(left + right);

    left >= 3 && right >= 3 && crossing * 10 <= column_lines.len()
}

/// Reading order for a two-column page: full-width lines act as band
/// separators (title, abstract, figure rows); within each band the left
/// column is read before the right.
fn order_two_column(page: &Page, lines: &[Line]) -> Vec<Line> {
    let content_left = lines.iter().map(|line| line.left).min().unwrap_or(0);
    let content_right = lines
        .iter()
        .map(|line| line.right)
        .max()
        .unwrap_or(page.width);
    let content_width = (content_right - content_left).max(1);
    let mid = content_left + content_width / 2;

    let mut sorted: Vec<&Line> = lines.iter().collect();
    sorted.sort_by_key(|line| (line.top, line.left));

    let mut ordered = Vec::with_capacity(lines.len());
    let mut band_left: Vec<&Line> = Vec::new();
    let mut band_right: Vec<&Line> = Vec::new();

    let flush =
        |ordered: &mut Vec<Line>, band_left: &mut Vec<&Line>, band_right: &mut Vec<&Line>| {
            for line in band_left.drain(..) {
                ordered.push(line.clone());
            }
            for line in band_right.drain(..) {
                ordered.push(line.clone());
            }
        };

    for line in sorted {
        if line.width() >= (content_width as f32 * 0.62) as i32 {
            flush(&mut ordered, &mut band_left, &mut band_right);
            ordered.push(line.clone());
        } else if line.left + line.width() / 2 <= mid {
            band_left.push(line);
        } else {
            band_right.push(line);
        }
    }
    flush(&mut ordered, &mut band_left, &mut band_right);

    ordered
}

/// The dominant body font size, weighted by character volume.
fn body_font_size(pages: &[Page]) -> u32 {
    let mut weights: HashMap<u32, usize> = HashMap::new();
    for page in pages {
        for fragment in &page.fragments {
            let size = page
                .font_sizes
                .get(&fragment.font)
                .copied()
                .unwrap_or(fragment.height.unsigned_abs());
            *weights.entry(size).or_default() += fragment.char_count();
        }
    }
    weights
        .into_iter()
        .max_by_key(|(_, chars)| *chars)
        .map(|(size, _)| size)
        .unwrap_or(12)
}

/// Distinct font sizes clearly larger than body text, ranked largest
/// first, mapped to heading levels h1..h3.
///
/// Only sizes actually used by fragments qualify: a `<fontspec>` can be
/// declared for every page while never being referenced by any `<text>`
/// element, and mapping such a phantom size to a heading level would
/// shift real headings down a rank (docs/report.md §4.5 PDF-8).
///
/// When there are more such sizes than levels, sizes that recur on two or
/// more pages win the levels over sizes seen on one page only: those are
/// display type on a cover or title page, and a book whose title pages
/// used two one-off sizes above its chapter titles had every section
/// heading demoted to a paragraph. A one-page size larger than every
/// ranked size still reads as a heading, at h1.
fn heading_levels(pages: &[Page], body_size: u32) -> HashMap<u32, u8> {
    const LEVELS: usize = 3;
    let mut page_counts: HashMap<u32, usize> = HashMap::new();
    for page in pages {
        let sizes: HashSet<u32> = page
            .fragments
            .iter()
            .filter(|fragment| is_on_page(page, fragment))
            .map(|fragment| fragment_font_size(page, fragment))
            .filter(|size| *size as f32 >= body_size as f32 * 1.15 && *size >= body_size + 2)
            .collect();
        for size in sizes {
            *page_counts.entry(size).or_default() += 1;
        }
    }
    let mut sizes: Vec<u32> = page_counts.keys().copied().collect();
    sizes.sort_unstable_by(|a, b| b.cmp(a));
    let (recurring, one_page): (Vec<u32>, Vec<u32>) = sizes
        .iter()
        .copied()
        .partition(|size| page_counts[size] >= 2);
    let mut ranked: Vec<u32> = recurring.into_iter().chain(one_page).take(LEVELS).collect();
    ranked.sort_unstable_by(|a, b| b.cmp(a));
    let top = ranked.first().copied().unwrap_or(u32::MAX);
    let mut levels: HashMap<u32, u8> = sizes
        .iter()
        .filter(|size| **size > top)
        .map(|size| (*size, 1))
        .collect();
    levels.extend(
        ranked
            .into_iter()
            .enumerate()
            .map(|(rank, size)| (size, rank as u8 + 1)),
    );
    levels
}

/// Most digits a note label can have: a year opening a line is prose, not
/// note 1984.
const NOTE_LABEL_MAX_DIGITS: usize = 3;

/// Lines that open an entry in a run of numbered notes. Each note starts on
/// its own line, but one-line notes share a left edge and the ordinary
/// leading, so neither the gap nor the indent rule separated them and a
/// page's notes read as one paragraph. A label counts when the next label,
/// or the one after it (a wrapped note line can open with a number too),
/// continues its sequence at the same left edge and size; and only where
/// the line before it ends early, since prose that wraps onto a number at
/// line start runs on to the measure.
fn numbered_note_starts(lines: &[Line], heading_levels: &HashMap<u32, u8>) -> HashSet<usize> {
    let labels: Vec<(usize, u32)> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| !heading_levels.contains_key(&line.font_size))
        .filter_map(|(index, line)| note_label_number(line).map(|number| (index, number)))
        .collect();
    let mut starts = HashSet::new();
    for (position, &(first, number)) in labels.iter().enumerate() {
        for &(second, next) in labels.iter().skip(position + 1).take(2) {
            if next == number + 1
                && lines[first].font_size == lines[second].font_size
                && lines[first].left.abs_diff(lines[second].left) <= 2
            {
                starts.extend([first, second]);
            }
        }
    }
    starts.retain(|&index| index == 0 || ends_early(lines, index - 1));
    starts
}

/// The number a line opens with when it reads as a note label: "12. ",
/// "12: " or "12) ", "[12]", a raised "12" (a superscript span or
/// superscript digits), or "(12)" or a bare "12 " before a word that is not
/// lowercase, the forms running prose also uses. Text must follow the label,
/// so "1.1 Scope" and "3.14 is" are not labels.
fn note_label_number(line: &Line) -> Option<u32> {
    let text = line.text();
    let text = text.trim_start();
    let raised = line
        .spans
        .iter()
        .find(|span| !span.text.trim().is_empty())
        .is_some_and(|span| span.superscript);
    let (open, text) = match text.strip_prefix(['[', '(']) {
        Some(rest) => (text.chars().next(), rest),
        None => (None, text),
    };
    let label_len = text
        .find(|ch: char| label_digit(ch).is_none())
        .unwrap_or(text.len());
    let label = &text[..label_len];
    if !(1..=NOTE_LABEL_MAX_DIGITS).contains(&label.chars().count()) {
        return None;
    }
    let rest = &text[label_len..];
    let reads_as_prose = |text: &str| {
        text.trim_start()
            .starts_with(|ch: char| ch.is_lowercase() || ch.is_ascii_digit())
    };
    let entry = match open {
        Some('[') => rest.strip_prefix(']')?,
        Some(_) => rest
            .strip_prefix(')')
            .filter(|rest| !reads_as_prose(rest))?,
        None if raised || !label.chars().all(|ch| ch.is_ascii_digit()) => rest,
        None => match rest.strip_prefix(['.', ':', ')']) {
            Some(rest) => rest.strip_prefix(char::is_whitespace)?,
            None => rest
                .strip_prefix(char::is_whitespace)
                .filter(|rest| !reads_as_prose(rest))?,
        },
    };
    if entry.trim().is_empty() {
        return None;
    }
    label
        .chars()
        .try_fold(0, |number, ch| Some(number * 10 + label_digit(ch)?))
}

/// The value of an ASCII or superscript digit.
fn label_digit(ch: char) -> Option<u32> {
    match ch {
        '\u{00b9}' => Some(1),
        '\u{00b2}' => Some(2),
        '\u{00b3}' => Some(3),
        '\u{2070}' | '\u{2074}'..='\u{2079}' => Some(ch as u32 - 0x2070),
        _ => ch.to_digit(10),
    }
}

/// Whether `lines[index]` stops more than its own height short of the
/// measure (the furthest right edge among lines starting no further right
/// than it, so a column is measured on its own) or closes a sentence.
fn ends_early(lines: &[Line], index: usize) -> bool {
    let line = &lines[index];
    let measure = lines
        .iter()
        .filter(|other| other.left <= line.left + line.height)
        .map(|other| other.right)
        .max()
        .unwrap_or(line.right);
    line.right < measure - line.height || ends_sentence_terminal(&line.text())
}

/// Cluster ordered lines into paragraphs and headings.
fn cluster_paragraphs(
    lines: &[Line],
    body_size: u32,
    heading_levels: &HashMap<u32, u8>,
) -> Vec<PendingBlock> {
    let median_gap = median_line_gap(lines);
    let note_starts = numbered_note_starts(lines, heading_levels);
    let mut blocks = Vec::new();
    let mut current: Vec<Span> = Vec::new();
    let mut current_heading: Option<u8> = None;
    let mut current_top: Option<i32> = None;
    let mut current_left: Option<i32> = None;
    let mut current_right: Option<i32> = None;
    let mut current_size: Option<u32> = None;
    let mut previous: Option<&Line> = None;

    let flush = |spans: &mut Vec<Span>,
                 heading: &mut Option<u8>,
                 top: &mut Option<i32>,
                 left: &mut Option<i32>,
                 right: &mut Option<i32>,
                 size: &mut Option<u32>,
                 blocks: &mut Vec<PendingBlock>| {
        if spans.is_empty() {
            return;
        }
        let spans = std::mem::take(spans);
        let block = match heading.take() {
            Some(level) => DocBlock::Heading { level, spans },
            None => DocBlock::Paragraph { spans },
        };
        let left = left.take().unwrap_or_default();
        let right = right.take().unwrap_or(left + 1).max(left + 1);
        blocks.push(PendingBlock {
            block,
            top: top.take().unwrap_or_default(),
            left,
            right,
            font_size: size.take().unwrap_or_default(),
        });
    };

    for (index, line) in lines.iter().enumerate() {
        let heading = heading_levels.get(&line.font_size).copied();
        let new_block = match previous {
            None => true,
            Some(prev) if heading.is_some() && line.font_size == prev.font_size => {
                // A wrapped heading: centred lines start at different offsets
                // and its leading exceeds body spacing, so neither the indent
                // nor the body-gap rule applies. Only a break of more than
                // twice the line's own height starts a new heading.
                let gap = line.top - prev.top;
                gap < 0 || gap > prev.height.max(1) * 2
            }
            Some(prev) => {
                let gap = line.top - prev.top;
                let size_changed = line.font_size != prev.font_size
                    && (heading.is_some()
                        || heading_levels.contains_key(&prev.font_size)
                        || line.font_size.abs_diff(prev.font_size) >= 2);
                let large_gap = gap > (median_gap as f32 * 1.8) as i32 || gap < 0;
                let indented =
                    line.left > prev.left + (body_size as i32) / 2 && prev.left <= line.left; // fresh indent, not a continuation of one
                size_changed || large_gap || indented || note_starts.contains(&index)
            }
        };

        if new_block {
            flush(
                &mut current,
                &mut current_heading,
                &mut current_top,
                &mut current_left,
                &mut current_right,
                &mut current_size,
                &mut blocks,
            );
            current_heading = heading;
            current_size = Some(line.font_size);
            current_top = Some(line.top);
            current_left = Some(line.left);
            current_right = Some(line.right);
        } else {
            current_left = Some(current_left.map_or(line.left, |left| left.min(line.left)));
            current_right = Some(current_right.map_or(line.right, |right| right.max(line.right)));
        }
        join_line_into(&mut current, line);
        previous = Some(line);
    }
    flush(
        &mut current,
        &mut current_heading,
        &mut current_top,
        &mut current_left,
        &mut current_right,
        &mut current_size,
        &mut blocks,
    );

    blocks
}

/// Classic line-end dehyphenation rules (docs/report.md §4.5 PDF-8):
/// a trailing hyphen is only fused away when it directly follows a
/// lowercase letter (not an em/en dash, numeric range or compound
/// marker) and the continuation starts with a lowercase letter. Any
/// other intra-line hyphen is left exactly as printed.
fn fuses_line_end_hyphen(tail_text: &str, head_text: &str) -> bool {
    let trimmed_tail = tail_text.trim_end();
    let mut chars = trimmed_tail.chars().rev();
    if chars.next() != Some('-') {
        return false;
    }
    if !chars.next().is_some_and(|ch| ch.is_lowercase()) {
        return false;
    }
    head_text
        .chars()
        .find(|ch| !ch.is_whitespace())
        .is_some_and(|ch| ch.is_lowercase())
}

/// CJK ideographs, kana, hangul syllables, Arabic and Hebrew have no
/// letter case; treat them as eligible continuations for the mechanical
/// cross-page paragraph merge when the previous text does not end in any
/// sentence terminal.
fn starts_caseless_script(head_text: &str) -> bool {
    head_text
        .chars()
        .find(|ch| !ch.is_whitespace())
        .is_some_and(|first| {
            matches!(first as u32,
                0x3040..=0x30FF          // hiragana + katakana
                | 0x3400..=0x4DBF        // CJK extension A
                | 0x4E00..=0x9FFF        // CJK unified ideographs
                | 0xAC00..=0xD7A3        // hangul syllables
                | 0xF900..=0xFAFF        // CJK compatibility ideographs
                | 0x0600..=0x06FF        // Arabic block (PDF F-2)
                | 0xFB50..=0xFEFF        // Arabic presentation forms A/B —
                                         // shaper-emitted ligature glyphs (PDF F-2)
                | 0x0590..=0x05FF        // Hebrew block (PDF F-2)
            )
        })
}

/// Whether a character sits anywhere in the CJK sphere (ideographs,
/// kana, hangul, full-width forms), used for adjacency decisions.
fn is_cjk_family_char(ch: char) -> bool {
    matches!(ch as u32,
        0x2E80..=0x9FFF   // radicals, kana, ideographs (incl. 3000-303F punct)
        | 0xAC00..=0xD7A3 // hangul
        | 0xF900..=0xFAFF // compatibility ideographs
        | 0xFF00..=0xFFEF // full-width forms  ，！？
    )
}

/// Whether `text`'s last visible character is CJK-family.
fn ends_with_cjk_family(text: &str) -> bool {
    text.chars()
        .rev()
        .find(|ch| !ch.is_whitespace())
        .is_some_and(is_cjk_family_char)
}

/// Whether joining tail+head needs an interposed space. CJK prose has
/// none of English's word boundaries, and its punctuation already acts
/// as a separator, so two adjacent CJK runs concatenate directly.
///
/// Kinsoku wrapping (PDF-9) can push an attached closing mark onto the
/// START of the next line; following a CJK-family tail that mark reopens
/// no word boundary and glues on directly.
fn needs_joining_space(tail_text: &str, head_text: &str) -> bool {
    if starts_wrapped_cjk_punctuation(head_text) && ends_with_cjk_family(tail_text) {
        return false;
    }
    let head_is_cjk = head_text
        .chars()
        .find(|ch| !ch.is_whitespace())
        .is_some_and(is_cjk_family_char);
    if !head_is_cjk {
        return true;
    }
    match tail_text.chars().rev().find(|ch| !ch.is_whitespace()) {
        None => true,
        Some(last) => !is_cjk_family_char(last),
    }
}

/// Sentence terminals include the full-width/CJK repertoire so a CJK
/// sentence ending in 。！？； is never merged into the next page, plus
/// the Arabic full stop ۔ and Arabic question mark ؟ (PDF F-2).
///
/// Deliberately excluded (PDF F-2): the Arabic comma ، (U+060C) and the
/// Hebrew maqaf ‎־‎ (U+05BE). The Arabic comma separates clauses inside a
/// single sentence — treating it as a terminal would fragment legitimate
/// paragraphs at every page break — and the maqaf is an intra-word
/// hyphen-like separator inside Hebrew compounds (אב־הטיפוס), not a
/// sentence boundary. Both must let the cross-page merge continue.
fn ends_sentence_terminal(tail_text: &str) -> bool {
    tail_text.trim_end().ends_with([
        '.', '!', '?', ':', ';', '"', '\u{201d}', '\u{2019}', '\u{3002}', // 。
        '\u{ff01}', // ！
        '\u{ff1f}', // ？
        '\u{ff1a}', // ：
        '\u{ff1b}', // ；
        '\u{2026}', // …
        '\u{300d}', // 」
        '\u{300f}', // 』
        '\u{ff09}', // ）
        '\u{061f}', // ؟ Arabic question mark (PDF F-2)
        '\u{06d4}', // ۔ Arabic full stop (PDF F-2)
    ])
}

/// Punctuation that justification or kinsoku wrapping pushes onto the
/// start of a wrapped CJK line (PDF-9): attached closing marks, half of
/// a paired enclosure, or an ASCII fallback. Such a head belongs to the
/// sentence before it and must not terminate the paragraph even though
/// it is neither lowercase nor caseless-script alphabetic.
const WRAPPED_CJK_CONTINUATION_HEADS: &[char] = &[
    '\u{3001}', // 、
    '\u{3002}', // 。
    '\u{FF0C}', // ，
    '\u{FF0E}', // ．
    '\u{FF1A}', // ：
    '\u{FF1B}', // ；
    '\u{FF01}', // ！
    '\u{FF1F}', // ？
    '\u{300D}', // 」
    '\u{300F}', // 』
    '\u{3009}', // 〉
    '\u{300B}', // 》
    '\u{FF09}', // ）
    '\u{FF5D}', // ｝
    '\u{FF3D}', // ］
    '\u{3011}', // 】
    ',', '.', '!', '?', ':', ';', ')', ']', '}',
];

/// Whether a wrapped line opens with one of those attached marks.
fn starts_wrapped_cjk_punctuation(head_text: &str) -> bool {
    head_text
        .chars()
        .find(|ch| !ch.is_whitespace())
        .is_some_and(|first| WRAPPED_CJK_CONTINUATION_HEADS.contains(&first))
}

/// Whether `head`'s normalized text opens with the chapter-group prefix,
/// using exactly epub.rs's matching (`normalize_visible_text` +
/// lowercase + starts_with). Only textual chapter starts need this
/// guard: when the next page opens with a real heading block no merge
/// can happen because only Paragraph↔Paragraph continuations exist.
fn begins_chapter_group(head_text: &str, chapter_prefix: Option<&str>) -> bool {
    let Some(prefix) = chapter_prefix.map(str::trim).filter(|p| !p.is_empty()) else {
        return false;
    };
    let normalize = |text: &str| {
        text.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    };
    normalize(head_text).starts_with(&normalize(prefix))
}

/// Append a line's spans to a paragraph buffer, repairing soft hyphens
/// at the join.
fn join_line_into(buffer: &mut Vec<Span>, line: &Line) {
    if buffer.is_empty() {
        buffer.extend(line.spans.iter().cloned());
        return;
    }

    let last_ends_hyphen = buffer
        .last()
        .is_some_and(|span| span.text.trim_end().ends_with('-'));
    let head_starts_lower = line
        .text()
        .chars()
        .find(|ch| !ch.is_whitespace())
        .is_some_and(char::is_lowercase);

    if last_ends_hyphen && head_starts_lower {
        let buffer_text: String = buffer.iter().map(|span| span.text.as_str()).collect();
        if fuses_line_end_hyphen(&buffer_text, &line.text()) {
            if let Some(last) = buffer.last_mut() {
                let trimmed = last.text.trim_end().to_string();
                last.text = trimmed[..trimmed.len() - 1].to_string();
            }
        } else if !buffer.last().is_some_and(|span| span.text.ends_with(' ')) {
            push_joined(buffer, " ", false, false);
        }
    } else if !buffer.last().is_some_and(|span| span.text.ends_with(' ')) {
        let tail_is_cjk_run = {
            let text: String = buffer.iter().map(|span| span.text.as_str()).collect();
            !needs_joining_space(&text, &line.text())
        };
        if !tail_is_cjk_run {
            push_joined(buffer, " ", false, false);
        }
    }
    for span in &line.spans {
        push_styled(buffer, span.clone());
    }
}

fn median_line_gap(lines: &[Line]) -> i32 {
    let mut gaps: Vec<i32> = lines
        .windows(2)
        .filter_map(|pair| {
            let gap = pair[1].top - pair[0].top;
            let height = pair[0].height.max(1);
            // Only genuine line advances vote: superscript and subscript
            // fragments produce micro-gaps that would drag the median
            // down until ordinary leading looks like a paragraph break,
            // and figure whitespace would drag it up.
            (gap >= height / 2 && gap <= height * 4).then_some(gap)
        })
        .collect();
    if gaps.is_empty() {
        return 16;
    }
    gaps.sort_unstable();
    // Lower median: on short pages the gap list is tiny and the upper
    // median can land on the paragraph gap itself, hiding the break.
    gaps[(gaps.len() - 1) / 2]
}

/// Cross-page paragraph continuation: a page's first paragraph continues
/// the previous page's last one when the earlier text does not end a
/// sentence and the new text starts lowercase, in an uncased script
/// (so CJK prose keeps flowing across page breaks), or — justified-CJK
/// wrapping (PDF-9) — with attached closing punctuation that kinsoku
/// pushed onto the wrapped line. Merging never crosses a chapter-group
/// boundary opened by the configured prefix.
fn append_with_continuation(
    blocks: &mut Vec<AnchoredBlock>,
    block_sizes: &mut Vec<u32>,
    carried: &mut Vec<CarriedText>,
    page: u32,
    mut incoming: Vec<PendingBlock>,
    chapter_prefix: Option<&str>,
) {
    let target = incoming
        .first()
        .and_then(|head| continuation_target(blocks, block_sizes, head.font_size));
    let mut merged = false;
    if let (
        Some(AnchoredBlock {
            block: DocBlock::Paragraph { spans: tail },
            anchor,
        }),
        Some(PendingBlock {
            block: DocBlock::Paragraph { spans: head },
            ..
        }),
    ) = (target.map(|index| &mut blocks[index]), incoming.first_mut())
    {
        let tail_text: String = tail.iter().map(|span| span.text.as_str()).collect();
        let head_text: String = head.iter().map(|span| span.text.as_str()).collect();
        let continues = !ends_sentence_terminal(&tail_text)
            && !begins_chapter_group(&head_text, chapter_prefix)
            && (head_text.chars().next().is_some_and(|ch| ch.is_lowercase())
                || starts_caseless_script(&head_text)
                || starts_wrapped_cjk_punctuation(&head_text));
        if continues {
            if fuses_line_end_hyphen(&tail_text, &head_text) {
                if let Some(last) = tail.last_mut() {
                    let trimmed = last.text.trim_end().to_string();
                    last.text = trimmed[..trimmed.len() - 1].to_string();
                }
            } else if needs_joining_space(&tail_text, &head_text)
                && !tail.last().is_some_and(|span| span.text.ends_with(' '))
            {
                push_joined(tail, " ", false, false);
            }
            carried.push(CarriedText {
                page,
                anchor_page: anchor.page,
                chars: head
                    .iter()
                    .map(|span| count_visible_chars(&span.text))
                    .sum(),
            });
            for span in head.drain(..) {
                push_styled(tail, span);
            }
            merged = true;
        }
    }
    if merged {
        incoming.remove(0);
    }
    for anchored in incoming {
        block_sizes.push(anchored.font_size);
        blocks.push(AnchoredBlock {
            block: anchored.block,
            anchor: BlockAnchor {
                page,
                top: anchored.top,
                left: anchored.left,
                width: (anchored.right - anchored.left).max(1),
            },
        });
    }
}

/// The block a new page's opening text may continue: the previous page's
/// last block, unless that page ends in blocks set smaller than the new text
/// (footnotes, captions). Body text that ran over a page break below a
/// page's footnotes was glued onto the last footnote; it now continues the
/// last block before them set at least as large as itself.
fn continuation_target(blocks: &[AnchoredBlock], sizes: &[u32], head_size: u32) -> Option<usize> {
    let last = blocks.len().checked_sub(1)?;
    let last_page = blocks[last].anchor.page;
    let mut index = last;
    while index > 0 && sizes[index] < head_size && blocks[index].anchor.page == last_page {
        index -= 1;
    }
    Some(if sizes[index] < head_size {
        last
    } else {
        index
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_pdf2xml;

    fn fixture_two_column() -> String {
        // A miniature two-column paper page: full-width title, abstract,
        // then two columns whose reading order must be left-then-right,
        // with a hyphenated join inside the left column.
        let mut texts = String::new();
        texts.push_str(
            r#"<text top="80" left="150" width="620" height="26" font="0">A Study of <i>Synthetic</i> Layouts</text>"#,
        );
        texts.push_str(
            r#"<text top="140" left="120" width="680" height="13" font="1">Abstract spanning the full width of the page for testing purposes.</text>"#,
        );
        for (i, words) in [
            "Left column opening line about num-",
            "bers and continuation of thought.",
            "Another left line here.",
        ]
        .iter()
        .enumerate()
        {
            texts.push_str(&format!(
                r#"<text top="{}" left="100" width="330" height="12" font="1">{}</text>"#,
                220 + i * 16,
                words
            ));
        }
        for (i, words) in [
            "Right column first line.",
            "Right column second line.",
            "Right column third line.",
        ]
        .iter()
        .enumerate()
        {
            texts.push_str(&format!(
                r#"<text top="{}" left="480" width="330" height="12" font="1">{}</text>"#,
                220 + i * 16,
                words
            ));
        }
        format!(
            r#"<?xml version="1.0"?>
<pdf2xml producer="poppler">
<page number="1" width="918" height="1188">
<fontspec id="0" size="22" family="T"/>
<fontspec id="1" size="11" family="T"/>
{texts}
</page>
</pdf2xml>"#
        )
    }

    #[test]
    fn two_column_page_reads_title_left_then_right() {
        let pages = parse_pdf2xml(&fixture_two_column()).expect("fixture parses");
        let result = reconstruct(&pages, ColumnMode::Auto);

        assert!(result.pages[0].two_column, "page must detect two columns");
        let texts: Vec<String> = result
            .blocks
            .iter()
            .map(|anchored| anchored.block.text())
            .collect();

        assert!(
            matches!(&result.blocks[0].block, DocBlock::Heading { level: 1, .. }),
            "title should be an h1, got {:?}",
            result.blocks[0]
        );
        assert_eq!(texts[0], "A Study of Synthetic Layouts");
        assert!(texts[1].starts_with("Abstract spanning"));
        assert!(
            texts[2].contains("numbers and continuation"),
            "hyphenated line join must repair the soft hyphen, got: {}",
            texts[2]
        );
        assert!(
            texts[2].contains("Another left line"),
            "left column must precede right, got: {texts:?}"
        );
        assert!(texts[3].starts_with("Right column first"));
    }

    #[test]
    fn single_column_page_clusters_paragraphs_by_gap() {
        let xml = r#"<?xml version="1.0"?>
<pdf2xml><page number="1" width="600" height="800">
<fontspec id="0" size="11" family="T"/>
<text top="100" left="80" width="440" height="12" font="0">First paragraph line one.</text>
<text top="116" left="80" width="440" height="12" font="0">First paragraph line two.</text>
<text top="170" left="80" width="440" height="12" font="0">Second paragraph after a large gap.</text>
</page></pdf2xml>"#;
        let pages = parse_pdf2xml(xml).expect("fixture parses");
        let result = reconstruct(&pages, ColumnMode::Auto);

        let texts: Vec<String> = result
            .blocks
            .iter()
            .map(|anchored| anchored.block.text())
            .collect();
        assert_eq!(
            texts,
            vec![
                "First paragraph line one. First paragraph line two.".to_string(),
                "Second paragraph after a large gap.".to_string(),
            ]
        );
    }

    #[test]
    fn narrow_gutter_does_not_merge_columns_on_aligned_baselines() {
        // Regression for the BERT page-1 defect: left and right column
        // lines sharing the exact same `top`, separated by a gutter of
        // only ~1.7x line height (461 - 435 = 26 at height 15). Merging
        // them creates a fake full-width line that scrambles band order.
        let xml = r#"<?xml version="1.0"?>
<pdf2xml><page number="1" width="892" height="1262">
<fontspec id="0" size="15" family="T"/>
<text top="100" left="108" width="327" height="15" font="0">left one alpha beta gamma delta.</text>
<text top="120" left="108" width="327" height="15" font="0">left two epsilon zeta eta theta.</text>
<text top="100" left="461" width="327" height="15" font="0">right one iota kappa lambda mu.</text>
<text top="120" left="461" width="327" height="15" font="0">right two nu xi omicron pi rho.</text>
<text top="140" left="108" width="327" height="15" font="0">left three sigma tau upsilon phi.</text>
<text top="140" left="461" width="327" height="15" font="0">right three chi psi omega end.</text>
<text top="160" left="108" width="327" height="15" font="0">left four extra line for detection.</text>
<text top="160" left="461" width="327" height="15" font="0">right four extra line as well.</text>
</page></pdf2xml>"#;
        let pages = parse_pdf2xml(xml).expect("fixture parses");
        let result = reconstruct(&pages, ColumnMode::Two);

        let all_text: String = result
            .blocks
            .iter()
            .map(|anchored| anchored.block.text())
            .collect::<Vec<_>>()
            .join("\n");
        let left_pos = all_text.find("left four").expect("left text present");
        let right_pos = all_text.find("right one").expect("right text present");
        assert!(
            left_pos < right_pos,
            "every left-column line must precede the right column, got:\n{all_text}"
        );
        assert!(
            !all_text.contains("alpha beta gamma delta. right one"),
            "aligned baselines must not merge across the gutter:\n{all_text}"
        );
    }

    #[test]
    fn paragraph_continues_across_pages() {
        let xml = r#"<?xml version="1.0"?>
<pdf2xml>
<page number="1" width="600" height="800">
<fontspec id="0" size="11" family="T"/>
<text top="700" left="80" width="440" height="12" font="0">This sentence does not end</text>
</page>
<page number="2" width="600" height="800">
<fontspec id="0" size="11" family="T"/>
<text top="80" left="80" width="440" height="12" font="0">until the following page.</text>
</page>
</pdf2xml>"#;
        let pages = parse_pdf2xml(xml).expect("fixture parses");
        let result = reconstruct(&pages, ColumnMode::Auto);

        let texts: Vec<String> = result
            .blocks
            .iter()
            .map(|anchored| anchored.block.text())
            .collect();
        assert_eq!(
            texts,
            vec!["This sentence does not end until the following page.".to_string()]
        );
    }

    fn texts(result: &Reconstruction) -> Vec<String> {
        result
            .blocks
            .iter()
            .map(|anchored| anchored.block.text())
            .collect()
    }

    /// Pages whose `(folio, body, top)` sit as in a typeset book: the folio
    /// alone in a margin, set a little larger than the body, which sits
    /// below the margin band.
    fn single_line_pages_xml(pages: &[(&str, &str, i32)]) -> String {
        let mut xml = String::from("<?xml version=\"1.0\"?>\n<pdf2xml>\n");
        for (index, (folio, body, folio_top)) in pages.iter().enumerate() {
            xml.push_str(&format!(
                "<page number=\"{}\" width=\"600\" height=\"800\">\n\
                 <fontspec id=\"0\" size=\"11\" family=\"T\"/>\n\
                 <fontspec id=\"1\" size=\"13\" family=\"T\"/>\n\
                 <text top=\"{folio_top}\" left=\"295\" width=\"16\" height=\"14\" font=\"1\">{folio}</text>\n\
                 <text top=\"120\" left=\"80\" width=\"440\" height=\"12\" font=\"0\">{body}</text>\n\
                 </page>\n",
                index + 1
            ));
        }
        xml.push_str("</pdf2xml>");
        xml
    }

    #[test]
    fn page_folios_are_dropped_and_sentences_join_across_them() {
        // A folio at the top of each page used to become the next page's
        // first block, so the sentence below it was cut at every page break
        // and a word hyphenated across the break stayed split.
        let xml = single_line_pages_xml(&[
            (
                "11",
                "The opening sentence of this invented book runs past the con-",
                40,
            ),
            (
                "12",
                "figurations of its first page and has not stopped at the end of a",
                40,
            ),
            ("13", "second page, ending only here.", 40),
        ]);
        let pages = parse_pdf2xml(&xml).expect("fixture parses");
        let result = reconstruct(&pages, ColumnMode::Auto);

        assert_eq!(
            texts(&result),
            vec![
                "The opening sentence of this invented book runs past the configurations of \
                 its first page and has not stopped at the end of a second page, ending only \
                 here."
                    .to_string()
            ]
        );
        let removed: Vec<usize> = result
            .pages
            .iter()
            .map(|stats| stats.running_header_chars)
            .collect();
        assert_eq!(removed, vec![2, 2, 2], "folios count as margin furniture");
    }

    #[test]
    fn roman_front_matter_folios_are_dropped() {
        let xml = single_line_pages_xml(&[
            ("i", "Preface page one.", 760),
            ("ii", "Preface page two.", 760),
            ("iii", "Preface page three.", 760),
        ]);
        let pages = parse_pdf2xml(&xml).expect("fixture parses");
        let result = reconstruct(&pages, ColumnMode::Auto);

        assert_eq!(
            texts(&result),
            vec![
                "Preface page one.",
                "Preface page two.",
                "Preface page three."
            ]
        );
    }

    #[test]
    fn margin_numbers_without_a_page_sequence_stay_text() {
        // Margin numbers whose values do not track the page distance, and a
        // bare year in the body, are content rather than folios.
        let xml = r#"<?xml version="1.0"?>
<pdf2xml>
<page number="1" width="600" height="800">
<fontspec id="0" size="11" family="T"/>
<text top="40" left="295" width="12" height="12" font="0">7</text>
<text top="120" left="80" width="440" height="12" font="0">First page prose ends here.</text>
<text top="400" left="80" width="40" height="12" font="0">1984</text>
</page>
<page number="2" width="600" height="800">
<fontspec id="0" size="11" family="T"/>
<text top="40" left="295" width="12" height="12" font="0">42</text>
<text top="120" left="80" width="440" height="12" font="0">Second page prose ends here.</text>
</page>
</pdf2xml>"#;
        let pages = parse_pdf2xml(xml).expect("fixture parses");
        let texts = texts(&reconstruct(&pages, ColumnMode::Auto));

        for kept in ["7", "1984", "42"] {
            assert!(
                texts.iter().any(|text| text == kept),
                "{kept} must stay: {texts:?}"
            );
        }
    }

    #[test]
    fn centred_heading_lines_form_one_heading() {
        // A title wrapped over three centred lines with heading leading used
        // to become three headings, translated and listed in the navigation
        // one fragment at a time. Same-size headings far apart stay separate.
        let xml = r#"<?xml version="1.0"?>
<pdf2xml><page number="1" width="600" height="800">
<fontspec id="0" size="11" family="T"/>
<fontspec id="1" size="22" family="T"/>
<text top="120" left="110" width="380" height="22" font="1">AN INVENTED TITLE THAT</text>
<text top="148" left="200" width="200" height="22" font="1">WRAPS ACROSS</text>
<text top="176" left="160" width="280" height="22" font="1">THREE CENTRED LINES</text>
<text top="260" left="80" width="440" height="12" font="0">Body prose follows the title and runs long enough to</text>
<text top="275" left="80" width="440" height="12" font="0">make eleven points the body size, as it is in any book.</text>
<text top="420" left="200" width="200" height="22" font="1">A LATER HEADING</text>
<text top="520" left="200" width="200" height="22" font="1">ANOTHER HEADING</text>
</page></pdf2xml>"#;
        let pages = parse_pdf2xml(xml).expect("fixture parses");
        let result = reconstruct(&pages, ColumnMode::Auto);

        assert_eq!(
            texts(&result),
            vec![
                "AN INVENTED TITLE THAT WRAPS ACROSS THREE CENTRED LINES",
                "Body prose follows the title and runs long enough to make eleven points the \
                 body size, as it is in any book.",
                "A LATER HEADING",
                "ANOTHER HEADING",
            ]
        );
        assert!(matches!(result.blocks[0].block, DocBlock::Heading { .. }));
    }

    fn superscripts(result: &Reconstruction) -> Vec<String> {
        result
            .blocks
            .iter()
            .flat_map(|anchored| match &anchored.block {
                DocBlock::Paragraph { spans } | DocBlock::Heading { spans, .. } => spans.clone(),
                _ => Vec::new(),
            })
            .filter(|span| span.superscript)
            .map(|span| span.text)
            .collect()
    }

    #[test]
    fn mid_line_note_marks_stay_in_their_sentence_as_superscripts() {
        // pdftohtml reports a raised note mark as its own line, below the
        // line's top; it used to become a paragraph of its own and cut the
        // sentence in two.
        let xml = r#"<?xml version="1.0"?>
<pdf2xml><page number="1" width="600" height="800">
<fontspec id="0" size="11" family="T"/>
<fontspec id="1" size="7" family="T"/>
<text top="120" left="80" width="440" height="12" font="0">A long invented sentence keeps going so that the</text>
<text top="135" left="80" width="200" height="12" font="0">body size is clear, and here it cites</text>
<text top="136" left="282" width="8" height="7" font="1">87</text>
<text top="135" left="292" width="200" height="12" font="0">and then carries on.</text>
</page></pdf2xml>"#;
        let pages = parse_pdf2xml(xml).expect("fixture parses");
        let result = reconstruct(&pages, ColumnMode::Auto);

        assert_eq!(
            texts(&result),
            vec![
                "A long invented sentence keeps going so that the body size is clear, and here \
                 it cites87 and then carries on."
            ]
        );
        assert_eq!(superscripts(&result), vec!["87"]);
    }

    #[test]
    fn line_end_note_marks_attach_without_trailing_space() {
        // The host line's reported width includes trailing spaces, so the
        // mark lies "inside" it; the spaces must not separate the mark.
        let xml = r#"<?xml version="1.0"?>
<pdf2xml><page number="1" width="600" height="800">
<fontspec id="0" size="11" family="T"/>
<fontspec id="1" size="7" family="T"/>
<text top="120" left="80" width="440" height="12" font="0">Another invented line ends on a quotation.”  </text>
<text top="121" left="330" width="8" height="7" font="1">83</text>
<text top="135" left="80" width="440" height="12" font="0">The next line continues the very same paragraph.</text>
</page></pdf2xml>"#;
        let pages = parse_pdf2xml(xml).expect("fixture parses");
        let result = reconstruct(&pages, ColumnMode::Auto);

        assert_eq!(
            texts(&result),
            vec![
                "Another invented line ends on a quotation.”83 The next line continues the very \
                 same paragraph."
            ]
        );
        assert_eq!(superscripts(&result), vec!["83"]);
    }

    #[test]
    fn lowered_small_digits_are_not_superscripts() {
        let xml = r#"<?xml version="1.0"?>
<pdf2xml><page number="1" width="600" height="800">
<fontspec id="0" size="11" family="T"/>
<fontspec id="1" size="7" family="T"/>
<text top="100" left="80" width="440" height="12" font="0">Body text sets the size of this invented page well.</text>
<text top="120" left="80" width="200" height="12" font="0">Water is written H</text>
<text top="126" left="282" width="6" height="7" font="1">2</text>
<text top="120" left="290" width="230" height="12" font="0">O in this invented line.</text>
</page></pdf2xml>"#;
        let pages = parse_pdf2xml(xml).expect("fixture parses");
        let result = reconstruct(&pages, ColumnMode::Auto);

        assert!(superscripts(&result).is_empty(), "{:?}", result.blocks);
    }

    fn visible(texts: &[String]) -> String {
        texts
            .concat()
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect()
    }

    #[test]
    fn a_run_of_numbered_notes_keeps_one_paragraph_per_note() {
        // Geometry follows a real book's page-foot notes: a point smaller
        // than the body, first lines indented, ordinary leading. Only an
        // entry followed by a wrapped line was split off (by the indent of
        // the next entry); one-line entries ran together into one paragraph.
        let xml = r#"<?xml version="1.0"?>
<pdf2xml><page number="1" width="594" height="918">
<fontspec id="0" size="15" family="T"/>
<fontspec id="1" size="14" family="T"/>
<text top="100" left="54" width="490" height="18" font="0">The body of this invented page runs for several lines so that its type is</text>
<text top="118" left="54" width="490" height="18" font="0">plainly the body size, with an ordinary measure and ordinary leading, and</text>
<text top="136" left="54" width="490" height="18" font="0">enough words that the notes below cannot outweigh it when the size of the</text>
<text top="154" left="54" width="490" height="18" font="0">body is decided, which is all it has to do here; it then ends a little short</text>
<text top="172" left="54" width="300" height="18" font="0">of the right margin, as a paragraph does.</text>
<text top="700" left="72" width="70" height="16" font="1">11. A. Writer, </text>
<text top="700" left="142" width="110" height="16" font="1"><i>An Invented Title</i></text>
<text top="700" left="252" width="270" height="16" font="1">, London: Nowhere Press, 1990, 41, and</text>
<text top="715" left="54" width="210" height="16" font="1">again in the second edition, 77</text>
<text top="733" left="72" width="100" height="16" font="1">12. See Writer, </text>
<text top="733" left="172" width="110" height="16" font="1"><i>Another Title</i></text>
<text top="733" left="282" width="74" height="16" font="1">, esp 170-182</text>
<text top="751" left="72" width="100" height="16" font="1">13. See Writer, </text>
<text top="751" left="172" width="110" height="16" font="1"><i>Another Title</i></text>
<text top="751" left="282" width="74" height="16" font="1">, esp 170-182</text>
<text top="769" left="72" width="80" height="16" font="1">14. Ibid., 9</text>
<text top="787" left="72" width="35" height="16" font="1">15. In </text>
<text top="787" left="107" width="256" height="16" font="1"><i>A Long Invented History of Things</i></text>
<text top="787" left="363" width="172" height="16" font="1">, Elsewhere: Small House, the</text>
<text top="802" left="54" width="270" height="16" font="1">author compares the two (224).</text>
</page></pdf2xml>"#;
        let pages = parse_pdf2xml(xml).expect("fixture parses");
        let result = reconstruct(&pages, ColumnMode::Auto);
        let texts = texts(&result);

        assert_eq!(
            texts[1..],
            [
                "11. A. Writer, An Invented Title, London: Nowhere Press, 1990, 41, and again \
                 in the second edition, 77",
                "12. See Writer, Another Title, esp 170-182",
                "13. See Writer, Another Title, esp 170-182",
                "14. Ibid., 9",
                "15. In A Long Invented History of Things, Elsewhere: Small House, the author \
                 compares the two (224).",
            ],
            "{texts:?}"
        );
        assert!(texts[0].starts_with("The body of this invented page"));
        let DocBlock::Paragraph { spans } = &result.blocks[3].block else {
            panic!("note 13 is a paragraph: {:?}", result.blocks[3]);
        };
        assert!(
            spans
                .iter()
                .any(|span| span.italic && span.text == "Another Title")
        );
        let source: String = pages[0]
            .fragments
            .iter()
            .flat_map(|fragment| &fragment.spans)
            .flat_map(|span| span.text.chars())
            .filter(|ch| !ch.is_whitespace())
            .collect();
        assert_eq!(visible(&texts), source);
    }

    #[test]
    fn bracketed_notes_at_body_size_without_indents_split_too() {
        // A chapter-end note list in body type, every line on one margin:
        // only the labels' sequence and the short line before each one
        // mark the entries. A wrapped entry keeps its full first line.
        let xml = r#"<?xml version="1.0"?>
<pdf2xml><page number="1" width="594" height="918">
<fontspec id="0" size="15" family="T"/>
<text top="100" left="54" width="250" height="18" font="0">[1] A. Writer, First Title, 12</text>
<text top="118" left="54" width="490" height="18" font="0">[2] B. Writer, Second Title, 1990, an entry that wraps onto a second line as</text>
<text top="136" left="54" width="120" height="18" font="0">it goes on, 33</text>
<text top="154" left="54" width="120" height="18" font="0">[3] Ibid., 34</text>
</page></pdf2xml>"#;
        let pages = parse_pdf2xml(xml).expect("fixture parses");
        let result = reconstruct(&pages, ColumnMode::Auto);

        assert_eq!(
            texts(&result),
            vec![
                "[1] A. Writer, First Title, 12",
                "[2] B. Writer, Second Title, 1990, an entry that wraps onto a second line as \
                 it goes on, 33",
                "[3] Ibid., 34",
            ]
        );
    }

    #[test]
    fn a_single_note_stays_whole_even_when_a_wrapped_line_opens_with_a_number() {
        // One note, no first-line indent. Its second line opens with "8.",
        // the next number, but follows a full line, so it continues note 7.
        let xml = r#"<?xml version="1.0"?>
<pdf2xml><page number="1" width="594" height="918">
<fontspec id="0" size="15" family="T"/>
<fontspec id="1" size="14" family="T"/>
<text top="100" left="54" width="490" height="18" font="0">The body of this invented page runs for several lines so that its type is</text>
<text top="118" left="54" width="490" height="18" font="0">plainly the body size, with an ordinary measure and ordinary leading, and</text>
<text top="136" left="54" width="300" height="18" font="0">then it stops short of the margin.</text>
<text top="700" left="54" width="486" height="16" font="1">7. See the long discussion of this argument in the third chapter, sections</text>
<text top="715" left="54" width="300" height="16" font="1">8. and 9., where the same claim returns.</text>
</page></pdf2xml>"#;
        let pages = parse_pdf2xml(xml).expect("fixture parses");
        let texts = texts(&reconstruct(&pages, ColumnMode::Auto));

        assert_eq!(
            texts[1..],
            [
                "7. See the long discussion of this argument in the third chapter, sections 8. \
              and 9., where the same claim returns."
            ],
            "{texts:?}"
        );
    }

    #[test]
    fn years_opening_lines_do_not_split_a_paragraph() {
        // Each year follows a closed sentence on a short line, so only the
        // three-digit limit on labels keeps this paragraph whole.
        let xml = r#"<?xml version="1.0"?>
<pdf2xml><page number="1" width="594" height="918">
<fontspec id="0" size="15" family="T"/>
<text top="100" left="54" width="430" height="18" font="0">Its first books came out slowly, and nobody kept records.</text>
<text top="118" left="54" width="490" height="18" font="0">1984. That changed when a second editor arrived and began to keep lists</text>
<text top="136" left="54" width="300" height="18" font="0">of everything printed that year.</text>
<text top="154" left="54" width="490" height="18" font="0">1985. The lists stop there, and nobody now knows why they were kept for</text>
<text top="172" left="54" width="120" height="18" font="0">one year only.</text>
</page></pdf2xml>"#;
        let pages = parse_pdf2xml(xml).expect("fixture parses");
        let texts = texts(&reconstruct(&pages, ColumnMode::Auto));

        assert_eq!(texts.len(), 1, "{texts:?}");
        assert!(texts[0].contains("records. 1984. That changed"));
        assert!(texts[0].contains("that year. 1985. The lists"));
    }

    #[test]
    fn a_numbered_list_running_through_prose_does_not_split_it() {
        // Justified prose wraps onto "1." and then "2.": consecutive labels
        // on one margin, but each follows a line that runs to the measure.
        let xml = r#"<?xml version="1.0"?>
<pdf2xml><page number="1" width="594" height="918">
<fontspec id="0" size="15" family="T"/>
<text top="100" left="54" width="490" height="18" font="0">Before a book is sent out, the editor confirms three things, in this order,</text>
<text top="118" left="54" width="490" height="18" font="0">1. that every page is present and in the right order, with nothing left out,</text>
<text top="136" left="54" width="430" height="18" font="0">2. that the notes match their marks, and 3. that the index is right.</text>
</page></pdf2xml>"#;
        let pages = parse_pdf2xml(xml).expect("fixture parses");
        let texts = texts(&reconstruct(&pages, ColumnMode::Auto));

        assert_eq!(texts.len(), 1, "{texts:?}");
        assert!(texts[0].contains("in this order, 1. that every page"));
        assert!(texts[0].contains("left out, 2. that the notes"));
    }

    #[test]
    fn note_labels_need_a_short_number_and_following_text() {
        let label = |spans: Vec<Span>| {
            note_label_number(&Line {
                top: 0,
                left: 0,
                right: 100,
                height: 12,
                font_size: 11,
                spans,
            })
        };
        let plain = |text: &str| {
            label(vec![Span {
                text: text.to_string(),
                bold: false,
                italic: false,
                superscript: false,
            }])
        };
        for (text, number) in [
            ("12. Author", 12),
            ("12: Author", 12),
            ("12) Author", 12),
            ("[12] Author", 12),
            ("[12]Author", 12),
            ("(12) Author", 12),
            ("12 Author", 12),
            ("12 “Title”", 12),
            ("¹² Author", 12),
            ("³Author", 3),
            ("  5. ibid.", 5),
        ] {
            assert_eq!(plain(text), Some(number), "{text}");
        }
        for text in [
            "1984. It was",
            "1.1 Scope",
            "3.14 is a decimal",
            "12:30 is a time",
            "12 of them",
            "12 34",
            "(2) the second",
            "12.",
            "12. ",
            "[12",
            "12th",
            "Chapter 12",
        ] {
            assert_eq!(plain(text), None, "{text}");
        }
        let raised = Span {
            text: "12".to_string(),
            bold: false,
            italic: false,
            superscript: true,
        };
        let entry = Span {
            text: "Author".to_string(),
            bold: false,
            italic: false,
            superscript: false,
        };
        assert_eq!(label(vec![raised.clone(), entry]), Some(12));
        assert_eq!(label(vec![raised]), None);
    }

    #[test]
    fn a_font_first_declared_mid_paragraph_does_not_split_it() {
        // Page 2 mixes a body font declared on page 1 with a same-size font
        // (say, the ligature subset) first declared on page 2. Fragment
        // heights differ from the declared size, as pdftohtml reports them.
        let xml = r#"<?xml version="1.0"?>
<pdf2xml>
<page number="1" width="600" height="800">
<fontspec id="0" size="15" family="Body"/>
<text top="120" left="80" width="440" height="18" font="0">The invented book opens with a finished sentence.</text>
</page>
<page number="2" width="600" height="800">
<fontspec id="1" size="15" family="BodyLigatures"/>
<text top="120" left="80" width="440" height="18" font="0">A paragraph on the second page runs into a</text>
<text top="138" left="80" width="440" height="18" font="1">line set in the newly declared font and then</text>
<text top="156" left="80" width="440" height="18" font="0">returns to the first font to finish.</text>
</page>
</pdf2xml>"#;
        let pages = parse_pdf2xml(xml).expect("fixture parses");
        let result = reconstruct(&pages, ColumnMode::Auto);

        assert_eq!(
            texts(&result),
            vec![
                "The invented book opens with a finished sentence.",
                "A paragraph on the second page runs into a line set in the newly declared font \
                 and then returns to the first font to finish.",
            ]
        );
    }

    #[test]
    fn running_header_chars_are_tracked_for_the_coverage_threshold() {
        // PDF-6: pages whose deficit is exactly the removed running
        // header must expose `running_header_chars` so the low-
        // confidence threshold can judge pre-removal coverage instead
        // of rasterizing/spending OCR on legitimate pages.
        let xml = r#"<?xml version="1.0"?>
<pdf2xml>
<page number="1" width="600" height="800">
<fontspec id="0" size="11" family="T"/>
<text top="40" left="80" width="180" height="12" font="0">WEEKLY GAZETTE</text>
<text top="120" left="80" width="440" height="12" font="0">First page prose starts here.</text>
</page>
<page number="2" width="600" height="800">
<fontspec id="0" size="11" family="T"/>
<text top="40" left="80" width="180" height="12" font="0">WEEKLY GAZETTE</text>
<text top="120" left="80" width="440" height="12" font="0">Second page body continues.</text>
</page>
</pdf2xml>"#;
        let pages = parse_pdf2xml(xml).expect("fixture parses");
        let result = reconstruct(&pages, ColumnMode::Auto);

        for stats in &result.pages {
            let header_chars = "WEEKLY GAZETTE"
                .chars()
                .filter(|ch| !ch.is_whitespace())
                .count();
            assert_eq!(
                stats.running_header_chars, header_chars,
                "header removal must be tracked per page: {:?}",
                result.pages
            );
        }
    }

    #[test]
    fn unused_fontspec_sizes_do_not_claim_heading_levels() {
        // PDF-8: a declared-but-unused fontspec size must not shift real
        // headings down the h1..h3 ranking.
        let xml = r#"<?xml version="1.0"?>
<pdf2xml><page number="1" width="600" height="800">
<fontspec id="0" size="11" family="T"/>
<fontspec id="1" size="30" family="T"/>
<fontspec id="2" size="16" family="T"/>
<text top="100" left="80" width="440" height="16" font="2">A Real Section Heading</text>
<text top="150" left="80" width="440" height="12" font="0">Body prose follows the heading.</text>
</page></pdf2xml>"#;
        let pages = parse_pdf2xml(xml).expect("fixture parses");
        let result = reconstruct(&pages, ColumnMode::Auto);

        let DocBlock::Heading { level, .. } = &result.blocks[0].block else {
            panic!("heading expected, got {:?}", result.blocks[0].block);
        };
        assert_eq!(*level, 1, "the largest USED size ranks first, got {level}");
    }

    #[test]
    fn one_page_display_sizes_do_not_take_levels_from_recurring_headings() {
        // A title page set in two one-off sizes above the chapter size used
        // to claim h1 and h2, leaving the section and subsection sizes with
        // no level: every section heading became a paragraph.
        let xml = r#"<?xml version="1.0"?>
<pdf2xml><page number="1" width="600" height="800">
<fontspec id="0" size="11" family="T"/>
<fontspec id="1" size="40" family="T"/>
<fontspec id="2" size="32" family="T"/>
<fontspec id="3" size="26" family="T"/>
<fontspec id="4" size="20" family="T"/>
<fontspec id="5" size="16" family="T"/>
<text top="200" left="100" width="400" height="40" font="1">An Invented Book</text>
<text top="300" left="100" width="400" height="32" font="2">Its Invented Subtitle</text>
</page>
<page number="2" width="600" height="800">
<text top="150" left="80" width="440" height="26" font="3">First Chapter</text>
<text top="200" left="80" width="440" height="20" font="4">1.1 A Section</text>
<text top="240" left="80" width="440" height="16" font="5">A Subsection</text>
<text top="280" left="80" width="440" height="12" font="0">Body prose runs long enough here to make eleven points the body size.</text>
</page>
<page number="3" width="600" height="800">
<text top="150" left="80" width="440" height="26" font="3">Second Chapter</text>
<text top="200" left="80" width="440" height="20" font="4">2.1 Another Section</text>
<text top="240" left="80" width="440" height="16" font="5">Another Subsection</text>
<text top="280" left="80" width="440" height="12" font="0">More body prose follows, again long enough to outweigh the headings.</text>
</page></pdf2xml>"#;
        let pages = parse_pdf2xml(xml).expect("fixture parses");
        let result = reconstruct(&pages, ColumnMode::Auto);

        // Heading level per block in reading order; 0 marks a paragraph.
        let levels: Vec<u8> = result
            .blocks
            .iter()
            .map(|anchored| match &anchored.block {
                DocBlock::Heading { level, .. } => *level,
                _ => 0,
            })
            .collect();
        assert_eq!(texts(&result)[1], "Its Invented Subtitle");
        assert_eq!(levels, [1, 1, 1, 2, 3, 0, 1, 2, 3, 0]);
    }

    /// A page with the given `(top, left, width, height, font, text)`
    /// fragments, followed by an empty page numbered 120 so page numbers up
    /// to that count are plausible.
    fn contents_fixture(fontspecs: &str, fragments: &[(i32, i32, i32, i32, u32, &str)]) -> String {
        let mut xml = format!(
            "<?xml version=\"1.0\"?>\n<pdf2xml><page number=\"1\" width=\"600\" height=\"800\">\n{fontspecs}\n"
        );
        for (top, left, width, height, font, text) in fragments {
            xml.push_str(&format!(
                "<text top=\"{top}\" left=\"{left}\" width=\"{width}\" height=\"{height}\" font=\"{font}\">{text}</text>\n"
            ));
        }
        xml.push_str(
            "</page>\n<page number=\"120\" width=\"600\" height=\"800\"></page></pdf2xml>",
        );
        xml
    }

    #[test]
    fn contents_page_numbers_are_dropped() {
        // A printed contents page sets each page number flush right on the
        // entry's first line. Each number became a paragraph of its own, or
        // ran into the next entry's text.
        let xml = contents_fixture(
            "<fontspec id=\"1\" size=\"13\" family=\"T\"/>\n<fontspec id=\"2\" size=\"10\" family=\"T\"/>",
            &[
                (100, 60, 80, 14, 1, "Preface"),
                (100, 530, 6, 14, 1, "v"),
                (140, 60, 90, 14, 1, "Introduction"),
                (140, 528, 8, 14, 1, "2"),
                (
                    180,
                    60,
                    380,
                    14,
                    1,
                    "An Invented Chapter Whose Title Runs On",
                ),
                (180, 520, 16, 14, 1, "11"),
                (196, 60, 160, 14, 1, "onto a Second Line"),
                (220, 60, 400, 12, 2, "First Section — Second Section —"),
                (234, 60, 400, 12, 2, "Third Section — Fourth Section —"),
                (248, 60, 200, 12, 2, "Fifth Section"),
                (290, 60, 300, 14, 1, "Another Invented Chapter"),
                (290, 520, 16, 14, 1, "49"),
                (306, 60, 400, 12, 2, "Sixth Section — Seventh Section —"),
                (320, 60, 200, 12, 2, "Eighth Section"),
            ],
        );
        let pages = parse_pdf2xml(&xml).expect("fixture parses");
        let result = reconstruct(&pages, ColumnMode::Auto);

        assert_eq!(
            texts(&result),
            vec![
                "Preface",
                "Introduction",
                "An Invented Chapter Whose Title Runs On onto a Second Line",
                "First Section — Second Section — Third Section — Fourth Section — Fifth Section",
                "Another Invented Chapter",
                "Sixth Section — Seventh Section — Eighth Section",
            ]
        );
        assert_eq!(result.pages[0].running_header_chars, 6);
    }

    #[test]
    fn right_aligned_years_and_lone_numbers_are_kept() {
        // Figures past the page count are data, not page references, and two
        // right-aligned numbers are too few to call a page a contents page.
        let years = contents_fixture(
            "<fontspec id=\"1\" size=\"11\" family=\"T\"/>",
            &[
                (100, 60, 300, 12, 1, "Founding of an invented society"),
                (100, 510, 26, 12, 1, "1901"),
                (120, 60, 300, 12, 1, "First invented congress"),
                (120, 510, 26, 12, 1, "1923"),
                (140, 60, 300, 12, 1, "Second invented congress"),
                (140, 510, 26, 12, 1, "1958"),
            ],
        );
        let result = reconstruct(
            &parse_pdf2xml(&years).expect("fixture parses"),
            ColumnMode::Auto,
        );
        assert!(
            ["1901", "1923", "1958"]
                .iter()
                .all(|year| texts(&result).concat().contains(year)),
            "{:?}",
            texts(&result)
        );

        let two = contents_fixture(
            "<fontspec id=\"1\" size=\"11\" family=\"T\"/>",
            &[
                (100, 60, 300, 12, 1, "An invented entry"),
                (100, 520, 16, 12, 1, "11"),
                (140, 60, 300, 12, 1, "Another invented entry"),
                (140, 520, 16, 12, 1, "49"),
            ],
        );
        let result = reconstruct(
            &parse_pdf2xml(&two).expect("fixture parses"),
            ColumnMode::Auto,
        );
        assert!(
            texts(&result).concat().contains("11") && texts(&result).concat().contains("49"),
            "{:?}",
            texts(&result)
        );
    }

    #[test]
    fn body_text_running_on_below_footnotes_rejoins_its_paragraph() {
        // A page that ends a paragraph mid-word above its footnotes: the next
        // page's opening text was glued onto the last footnote.
        let xml = r#"<?xml version="1.0"?>
<pdf2xml><page number="1" width="600" height="800">
<fontspec id="0" size="11" family="T"/>
<fontspec id="1" size="9" family="T"/>
<text top="100" left="60" width="440" height="13" font="0">Invented body prose runs long enough here to make eleven points the</text>
<text top="116" left="60" width="440" height="13" font="0">body size of this page, and the sentence it starts as-</text>
<text top="700" left="60" width="440" height="11" font="1">1. An invented note citing Somebody, An Invented Title (Some</text>
</page>
<page number="2" width="600" height="800">
<text top="100" left="60" width="440" height="13" font="0">sumes the rest of its sentence on the following page.</text>
</page></pdf2xml>"#;
        let pages = parse_pdf2xml(xml).expect("fixture parses");
        let result = reconstruct(&pages, ColumnMode::Auto);

        assert_eq!(
            texts(&result),
            vec![
                "Invented body prose runs long enough here to make eleven points the body size of \
                 this page, and the sentence it starts assumes the rest of its sentence on the \
                 following page.",
                "1. An invented note citing Somebody, An Invented Title (Some",
            ]
        );
        assert_eq!(result.carried.len(), 1);
        assert_eq!(result.carried[0].anchor_page, 1);
    }

    #[test]
    fn a_footnote_running_on_to_the_next_page_still_joins_its_note() {
        let xml = r#"<?xml version="1.0"?>
<pdf2xml><page number="1" width="600" height="800">
<fontspec id="0" size="11" family="T"/>
<fontspec id="1" size="9" family="T"/>
<text top="100" left="60" width="440" height="13" font="0">Invented body prose runs long enough here to make eleven points the</text>
<text top="116" left="60" width="440" height="13" font="0">body size of this page, and it ends its sentence.</text>
<text top="700" left="60" width="440" height="11" font="1">2. An invented note that runs</text>
</page>
<page number="2" width="600" height="800">
<text top="100" left="60" width="440" height="11" font="1">on to the next page.</text>
</page></pdf2xml>"#;
        let pages = parse_pdf2xml(xml).expect("fixture parses");
        let result = reconstruct(&pages, ColumnMode::Auto);

        assert_eq!(
            texts(&result),
            vec![
                "Invented body prose runs long enough here to make eleven points the body size of \
                 this page, and it ends its sentence.",
                "2. An invented note that runs on to the next page.",
            ]
        );
    }

    #[test]
    fn a_footnote_line_repeated_at_two_page_feet_is_not_a_running_footer() {
        // Two notes end with the same short line, and both fall in the bottom
        // margin band. The line repeats on two pages, so it was deleted as a
        // running footer. The real running head still goes, also on a chapter
        // opener where a large title sits close beneath it.
        let page = |number: u32, note: &str| {
            let mut xml = format!("<page number=\"{number}\" width=\"600\" height=\"800\">\n");
            xml.push_str("<text top=\"40\" left=\"200\" width=\"200\" height=\"13\" font=\"0\">An Invented Running Head</text>\n");
            if number == 1 {
                xml.push_str("<text top=\"56\" left=\"100\" width=\"400\" height=\"30\" font=\"2\">AN INVENTED CHAPTER</text>\n");
            }
            for row in 0..10 {
                xml.push_str(&format!(
                    "<text top=\"{}\" left=\"60\" width=\"440\" height=\"13\" font=\"0\">Invented body line {row} of page {number}, long enough to set the body size.</text>\n",
                    100 + row * 16
                ));
            }
            xml.push_str(&format!(
                "<text top=\"742\" left=\"60\" width=\"440\" height=\"11\" font=\"1\">{note} An invented note citing Somebody,</text>\n"
            ));
            xml.push_str("<text top=\"754\" left=\"60\" width=\"200\" height=\"11\" font=\"1\">the Same Invented Title, 189.</text>\n</page>\n");
            xml
        };
        let xml = format!(
            "<?xml version=\"1.0\"?>\n<pdf2xml>\n<fontspec id=\"0\" size=\"11\" family=\"T\"/>\n<fontspec id=\"1\" size=\"9\" family=\"T\"/>\n<fontspec id=\"2\" size=\"24\" family=\"T\"/>\n{}{}</pdf2xml>",
            page(1, "1."),
            page(2, "2.")
        );
        let pages = parse_pdf2xml(&xml).expect("fixture parses");
        let result = reconstruct(&pages, ColumnMode::Auto);
        let text = texts(&result).join("\n");

        assert_eq!(
            text.matches("the Same Invented Title, 189.").count(),
            2,
            "{text}"
        );
        assert!(!text.contains("Running Head"), "{text}");
        assert!(text.contains("AN INVENTED CHAPTER"), "{text}");
    }

    #[test]
    fn text_carried_onto_a_later_page_is_recorded_against_it() {
        let xml = r#"<?xml version="1.0"?>
<pdf2xml><page number="1" width="600" height="800">
<fontspec id="0" size="11" family="T"/>
<text top="400" left="80" width="440" height="12" font="0">A sentence that runs on past the</text>
</page>
<page number="2" width="600" height="800">
<text top="400" left="80" width="440" height="12" font="0">end of its page.</text>
</page></pdf2xml>"#;
        let pages = parse_pdf2xml(xml).expect("fixture parses");
        let result = reconstruct(&pages, ColumnMode::Auto);

        assert_eq!(
            texts(&result),
            vec!["A sentence that runs on past the end of its page."]
        );
        assert_eq!(
            result.carried,
            vec![CarriedText {
                page: 2,
                anchor_page: 1,
                chars: 13,
            }]
        );
    }

    #[test]
    fn text_past_the_page_edge_is_read_once() {
        // A title set across a two-page spread is reported on both pages,
        // shifted past the edge of the first, and was translated twice.
        let xml = r#"<?xml version="1.0"?>
<pdf2xml><page number="1" width="600" height="800">
<fontspec id="0" size="11" family="T"/>
<fontspec id="1" size="30" family="T"/>
<text top="200" left="80" width="440" height="12" font="0">The verso page closes its chapter here.</text>
<text top="300" left="690" width="400" height="30" font="1">A SPREAD TITLE</text>
</page>
<page number="2" width="600" height="800">
<text top="300" left="90" width="400" height="30" font="1">A SPREAD TITLE</text>
<text top="400" left="80" width="440" height="12" font="0">The recto page opens the next chapter.</text>
</page></pdf2xml>"#;
        let pages = parse_pdf2xml(xml).expect("fixture parses");
        let result = reconstruct(&pages, ColumnMode::Auto);

        assert_eq!(
            texts(&result),
            vec![
                "The verso page closes its chapter here.",
                "A SPREAD TITLE",
                "The recto page opens the next chapter.",
            ]
        );
        assert_eq!(result.offpage_dropped_fragments, 1);
    }

    #[test]
    fn hyphen_fusion_stays_classic_line_end_dehyphenation() {
        // PDF-8: fuse lowercase word-end hyphens with lowercase
        // continuations ("prepro-"+"cessing"), but never uppercase,
        // symbol or numeric-range hyphen ends like "X-"+"ray".
        let xml = r#"<?xml version="1.0"?>
<pdf2xml><page number="1" width="600" height="800">
<fontspec id="0" size="11" family="T"/>
<text top="100" left="80" width="440" height="12" font="0">The image was prepro-</text>
<text top="116" left="80" width="440" height="12" font="0">cessed before inspection.</text>
<text top="170" left="80" width="440" height="12" font="0">Signals in the X-</text>
<text top="186" left="80" width="440" height="12" font="0">ray band were examined.</text>
<text top="220" left="80" width="440" height="12" font="0">Coverage spans 1990-</text>
<text top="236" left="80" width="440" height="12" font="0">2001 in the tables.</text>
</page></pdf2xml>"#;
        let pages = parse_pdf2xml(xml).expect("fixture parses");
        let result = reconstruct(&pages, ColumnMode::Auto);

        let texts: Vec<String> = result
            .blocks
            .iter()
            .map(|anchored| anchored.block.text())
            .collect();
        let joined = texts.join("\n---\n");
        assert!(
            texts.iter().any(|t| t.contains("preprocessed")),
            "lowercase word-end hyphen fuses: {joined}"
        );
        assert!(
            !texts.iter().any(|t| t.contains("Xray")),
            "uppercase hyphen end must not fuse: {joined}"
        );
        assert!(
            !texts.iter().any(|t| t.contains("19902001")),
            "numeric ranges must never fuse: {joined}"
        );
        assert!(
            texts.iter().any(|t| t.contains("X- ray")),
            "kept intact with spacing preserved: {joined}"
        );
    }

    #[test]
    fn cjk_paragraphs_merge_across_pages_and_terminals_block() {
        // PDF-8 mechanical half: uncased scripts continue paragraphs
        // across page breaks without inventing spaces; CJK sentence
        // terminals stop the merge.
        let flowing = r#"<?xml version="1.0"?>
<pdf2xml>
<page number="1" width="600" height="800">
<fontspec id="0" size="11" family="T"/>
<text top="760" left="80" width="440" height="12" font="0">山间的清晨安静而清冷，雾气</text>
</page>
<page number="2" width="600" height="800">
<fontspec id="0" size="11" family="T"/>
<text top="80" left="80" width="440" height="12" font="0">缓缓地从谷底升起，漫过石阶。</text>
</page>
</pdf2xml>"#;
        let pages = parse_pdf2xml(flowing).expect("fixture parses");
        let result = reconstruct(&pages, ColumnMode::Auto);

        let texts: Vec<String> = result
            .blocks
            .iter()
            .map(|anchored| anchored.block.text())
            .collect();
        assert_eq!(texts.len(), 1, "{texts:?}");
        assert!(
            texts[0].contains("雾气缓缓地"),
            "CJK continuation merges with no invented space: {texts:?}"
        );

        let terminated = flowing.replace("雾气", "雾气。");
        let pages = parse_pdf2xml(&terminated).expect("fixture parses");
        let result = reconstruct(&pages, ColumnMode::Auto);

        let texts: Vec<String> = result
            .blocks
            .iter()
            .map(|anchored| anchored.block.text())
            .collect();
        assert_eq!(
            texts.len(),
            2,
            "terminal 。 must block the merge: {texts:?}"
        );
        assert!(
            !result.blocks[0].block.text().ends_with("升起，"),
            "first paragraph must end at its own terminal"
        );
    }

    #[test]
    fn arabic_and_hebrew_terminal_punctuation_classifies_correctly() {
        // PDF F-2: genuine Arabic sentence terminals end a sentence...
        assert!(ends_sentence_terminal("وأخيرًا انتهى الأمر۔"));
        assert!(ends_sentence_terminal("ماذا حدث؟"));
        // ...while the Arabic comma and the Hebrew maqaf are separators
        // inside a sentence / inside a word and must never end one.
        assert!(!ends_sentence_terminal("قال الخبير، ثم ذهب"));
        assert!(!ends_sentence_terminal("أبو الكلام"));
        assert!(!ends_sentence_terminal("אב־הטיפוס"));
    }

    #[test]
    fn arabic_paragraphs_merge_across_pages_and_terminals_block() {
        // PDF F-2: an Arabic book spanning two pages continues its
        // paragraph (not fragmenting) — after the PDF-7 logical-order
        // repair the page-1 tail ends in an Arabic comma (U+060C), a
        // clause separator that is NOT a sentence terminal. Fixture text
        // is in poppler's visual word order (logical reversed), mirroring
        // what the repair step consumes.
        let flowing = r#"<?xml version="1.0"?>
<pdf2xml>
<page number="1" width="600" height="800">
<fontspec id="0" size="11" family="T"/>
<text top="760" left="80" width="440" height="12" font="0">كانت الضجيج، عن بعيد وادٍ في</text>
</page>
<page number="2" width="600" height="800">
<fontspec id="0" size="11" family="T"/>
<text top="80" left="80" width="440" height="12" font="0">توقف. بلا تُروى الحكايات</text>
</page>
</pdf2xml>"#;
        let pages = parse_pdf2xml(flowing).expect("fixture parses");
        let result = reconstruct(&pages, ColumnMode::Auto);

        let texts: Vec<String> = result
            .blocks
            .iter()
            .map(|anchored| anchored.block.text())
            .collect();
        assert_eq!(texts.len(), 1, "Arabic comma tail must merge: {texts:?}");
        assert!(
            texts[0].contains("كانت الحكايات"),
            "the repaired logical sentence must continue across the page join: {texts:?}"
        );

        // Genuine sentence breaks hold: the Arabic full stop and the
        // Arabic question mark both block the merge (they land on the
        // repaired tail's end, exactly where the check looks).
        for terminal in ["۔", "؟"] {
            let terminated = flowing.replace("كانت الضجيج", &format!("كانت{terminal} الضجيج"));
            let pages = parse_pdf2xml(&terminated).expect("fixture parses");
            let result = reconstruct(&pages, ColumnMode::Auto);
            assert_eq!(
                result.blocks.len(),
                2,
                "terminal {terminal} must block the Arabic merge: {:?}",
                result
                    .blocks
                    .iter()
                    .map(|b| b.block.text())
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn hebrew_paragraphs_merge_across_pages_and_maqaf_is_not_terminal() {
        // PDF F-2: a Hebrew book spanning two pages continues its
        // paragraph even when the repaired tail ends with a maqaf
        // (U+05BE) — an intra-word hyphen-like separator, not a sentence
        // end. A real sentence stop still holds the paragraphs apart.
        // Fixture text is in poppler's visual word order.
        let flowing = r#"<?xml version="1.0"?>
<pdf2xml>
<page number="1" width="600" height="800">
<fontspec id="0" size="11" family="T"/>
<text top="760" left="80" width="440" height="12" font="0">אב־ה־ עבר אל המסע</text>
</page>
<page number="2" width="600" height="800">
<fontspec id="0" size="11" family="T"/>
<text top="80" left="80" width="440" height="12" font="0">בזריחה. החל ההרים</text>
</page>
</pdf2xml>"#;
        let pages = parse_pdf2xml(flowing).expect("fixture parses");
        let result = reconstruct(&pages, ColumnMode::Auto);

        let texts: Vec<String> = result
            .blocks
            .iter()
            .map(|anchored| anchored.block.text())
            .collect();
        assert_eq!(
            texts.len(),
            1,
            "a maqaf tail must not end the sentence: {texts:?}"
        );
        assert!(
            texts[0].contains("אב־ה־ ההרים"),
            "the repaired logical sentence must continue across the page join: {texts:?}"
        );

        let terminated = flowing.replace("אב־ה־ עבר אל המסע", "ההרים. עבר אל המסע");
        let pages = parse_pdf2xml(&terminated).expect("fixture parses");
        let result = reconstruct(&pages, ColumnMode::Auto);
        assert_eq!(
            result.blocks.len(),
            2,
            "a real sentence stop still blocks the Hebrew merge: {:?}",
            result
                .blocks
                .iter()
                .map(|b| b.block.text())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn justified_cjk_wrapped_punctuation_heads_continue_paragraphs() {
        // PDF-9 justified half: kinsoku wraps push attached closing marks
        // onto the START of a wrapped line. Such a head belongs to the
        // previous sentence even though it opens with punctuation.
        let justified = r#"<?xml version="1.0"?>
<pdf2xml>
<page number="1" width="600" height="800">
<fontspec id="0" size="11" family="T"/>
<text top="750" left="80" width="440" height="12" font="0">山道の途中で、桜並木が視界を</text>
</page>
<page number="2" width="600" height="800">
<fontspec id="0" size="11" family="T"/>
<text top="80" left="80" width="440" height="12" font="0">、いっさい遮るものなく広がる。</text>
</page>
</pdf2xml>"#;
        let pages = parse_pdf2xml(justified).expect("fixture parses");
        let result = reconstruct(&pages, ColumnMode::Auto);

        let texts: Vec<String> = result
            .blocks
            .iter()
            .map(|anchored| anchored.block.text())
            .collect();
        assert_eq!(
            texts.len(),
            1,
            "a wrapped 、 head must continue the paragraph: {texts:?}"
        );
        assert!(
            texts[0].contains("視界を、いっさい"),
            "the mark glues onto the CJK tail with no invented space: {texts:?}"
        );
        assert!(
            !texts[0].contains("視界を 、"),
            "kinsoku marks must not be space-separated from their sentence: {texts:?}"
        );

        // A fullwidth comma head behaves identically (simplified-Chinese
        // justification shape).
        let comma_head = justified.replace(
            "<text top=\"80\" left=\"80\" width=\"440\" height=\"12\" font=\"0\">、いっさい遮るものなく広がる。</text>",
            "<text top=\"80\" left=\"80\" width=\"440\" height=\"12\" font=\"0\">，游人渐次多了起来。</text>",
        );
        let pages = parse_pdf2xml(&comma_head).expect("fixture parses");
        let result = reconstruct(&pages, ColumnMode::Auto);
        let texts: Vec<String> = result
            .blocks
            .iter()
            .map(|anchored| anchored.block.text())
            .collect();
        assert_eq!(texts.len(), 1, "， head must continue too: {texts:?}");
        assert!(texts[0].contains("，游人"));
    }

    #[test]
    fn chapter_group_boundary_blocks_the_cross_page_merge() {
        // PDF-9 guard: even when the merge conditions hold (non-terminal
        // tail, caseless head), a configured chapter prefix must never be
        // crossed — chapter splitting matches the same prefix and would
        // otherwise swallow the boundary line.
        let boundary = r#"<?xml version="1.0"?>
<pdf2xml>
<page number="1" width="600" height="800">
<fontspec id="0" size="11" family="T"/>
<text top="750" left="80" width="440" height="12" font="0">昨夜の雨が止んだころ</text>
</page>
<page number="2" width="600" height="800">
<fontspec id="0" size="11" family="T"/>
<text top="80" left="80" width="440" height="12" font="0">第二章　観測日誌の続きから</text>
</page>
</pdf2xml>"#;

        // Without a prefix the mechanical rules DO join them (proves the
        // head text satisfies continuation).
        let pages = parse_pdf2xml(boundary).expect("fixture parses");
        let unguarded = reconstruct(&pages, ColumnMode::Auto);
        assert_eq!(unguarded.blocks.len(), 1, "{:?}", unguarded.blocks.len());

        // With the guard the boundary stays intact.
        let pages = parse_pdf2xml(boundary).expect("fixture parses");
        let guarded = reconstruct_with_chapter_guard(&pages, ColumnMode::Auto, Some("第二章"));
        assert_eq!(
            guarded.blocks.len(),
            2,
            "chapter prefix must block the merge: {:?}",
            guarded
                .blocks
                .iter()
                .map(|b| b.block.text())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn rtl_lines_emerge_in_logical_reading_order_and_flag_the_page() {
        // PDF-7 wiring test at reconstruction level: poppler emits each
        // fragment's characters logically, visual assembly scrambles
        // dominant-RTL lines wordwise, and the repair step restores them.
        // Expected values mirror the algorithmic units proven in
        // crate::bidi's own tests; this checks their integration point,
        // the rtl_dominant page flag, and that LTR lines pass untouched.
        let xml = r#"<?xml version="1.0"?>
<pdf2xml>
<page number="1" width="600" height="800">
<fontspec id="0" size="12" family="T"/>
<text top="100" left="80" width="440" height="14" font="0">السنوي تقريره 2024 عام في المعهد أصدر</text>
<text top="124" left="80" width="440" height="14" font="0">חמישי רביעי שלישי Tesseract OCR שני ראשון</text>
<text top="148" left="80" width="440" height="14" font="0">Plain English narrative continues below.</text>
</page>
</pdf2xml>"#;
        let pages = parse_pdf2xml(xml).expect("fixture parses");
        let result = reconstruct(&pages, ColumnMode::Auto);

        let text: String = result
            .blocks
            .iter()
            .map(|b| b.block.text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("أصدر المعهد في عام 2024 تقريره السنوي"),
            "Arabic tokens restore logical order with the number inline: {text}"
        );
        assert!(
            text.contains("ראשון שני Tesseract OCR שלישי רביעי חמישי"),
            "embedded LTR clusters keep internal order: {text}"
        );
        assert!(
            text.contains("Plain English narrative continues below."),
            "LTR line untouched"
        );
        assert!(
            result.pages[0].rtl_dominant,
            "the page reports its RTL dominance for report attribution"
        );

        // Control: an LTR-only document never flips anything and never
        // claims RTL dominance, so coverage/OCR decisions stay script-
        // blind apart from real character counts.
        let ltr_xml = xml
            .replace(
                "السنوي تقريره 2024 عام في المعهد أصدر",
                "The institute issued its annual report",
            )
            .replace(
                "חמישי רביעי שלישי Tesseract OCR שני ראשון",
                "fourth third second Tesseract OCR two one",
            );
        let pages = parse_pdf2xml(&ltr_xml).expect("control fixture parses");
        let result = reconstruct(&pages, ColumnMode::Auto);
        assert!(!result.pages[0].rtl_dominant);
        assert_eq!(
            result.blocks[0].block.text(),
            "The institute issued its annual report fourth third second Tesseract OCR two one Plain English narrative continues below."
        );
    }

    #[test]
    fn repeated_running_headers_are_removed_before_paragraph_clustering() {
        let xml = r#"<?xml version="1.0"?>
<pdf2xml>
<page number="1" width="600" height="800">
<fontspec id="0" size="11" family="T"/>
<fontspec id="1" size="24" family="T"/>
<text top="60" left="80" width="180" height="12" font="0">THIS SOVIET WORLD</text>
<text top="88" left="120" width="360" height="26" font="1">THIS SOVIET WORLD</text>
<text top="150" left="80" width="440" height="12" font="0">First page prose starts here</text>
</page>
<page number="2" width="600" height="800">
<fontspec id="0" size="11" family="T"/>
<fontspec id="1" size="24" family="T"/>
<text top="60" left="80" width="180" height="12" font="0">THIS SOVIET WORLD</text>
<text top="120" left="80" width="440" height="12" font="0">second page body continues.</text>
</page>
<page number="3" width="600" height="800">
<fontspec id="0" size="11" family="T"/>
<fontspec id="1" size="24" family="T"/>
<text top="60" left="80" width="180" height="12" font="0">THIS SOVIET WORLD</text>
<text top="120" left="80" width="440" height="12" font="0">Third page body starts here.</text>
</page>
</pdf2xml>"#;
        let pages = parse_pdf2xml(xml).expect("fixture parses");
        let result = reconstruct(&pages, ColumnMode::Auto);

        let texts: Vec<String> = result
            .blocks
            .iter()
            .map(|anchored| anchored.block.text())
            .collect();

        assert_eq!(texts[0], "THIS SOVIET WORLD");
        assert_eq!(
            texts[1],
            "First page prose starts here second page body continues."
        );
        assert_eq!(texts[2], "Third page body starts here.");
        assert_eq!(
            texts
                .iter()
                .filter(|text| text.as_str() == "THIS SOVIET WORLD")
                .count(),
            1,
            "only the real title should remain: {texts:?}"
        );
    }
}
