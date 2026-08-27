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
    ColumnMode, DocBlock, Fragment, Line, Page, Span, normalize_text_key, spans_text,
};

/// Per-page reconstruction diagnostics for the conversion report.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PageStats {
    pub page: u32,
    pub lines: usize,
    pub chars: usize,
    pub baseline_chars: usize,
    /// Non-whitespace characters removed as repeated running
    /// headers/footers. The low-confidence coverage threshold is judged
    /// against `chars + running_header_chars` (the pre-removal view) so
    /// legitimate pages with recurring headers are not rasterized into
    /// OCR just because the header text dominates the deficit
    /// (docs/report.md §4.5 PDF-6).
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

    let mut blocks: Vec<AnchoredBlock> = Vec::new();
    let mut stats = Vec::new();
    let mut rotated_dropped_fragments = 0usize;

    for page in pages {
        rotated_dropped_fragments += rotated_fragment_count(page);
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
        let mut running_header_chars = 0usize;
        ordered.retain(|line| {
            if is_running_margin_line(page, line, body_size, &running_margin_texts) {
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
        append_with_continuation(&mut blocks, page.number, page_blocks, chapter_prefix);
    }

    Reconstruction {
        blocks,
        pages: stats,
        rotated_dropped_fragments,
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

fn running_margin_texts(pages: &[Page], body_size: u32) -> HashSet<String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for page in pages {
        let mut seen_on_page = HashSet::new();
        for line in merge_fragments_into_lines(page) {
            if line.font_size > body_size + 1 || !near_vertical_margin(page, &line) {
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
    line: &Line,
    body_size: u32,
    running_margin_texts: &HashSet<String>,
) -> bool {
    line.font_size <= body_size + 1
        && near_vertical_margin(page, line)
        && running_margin_texts.contains(&normalize_text_key(&line.text()))
}

fn near_vertical_margin(page: &Page, line: &Line) -> bool {
    let margin = (page.height / 8).max(line.height * 3);
    line.top <= margin || line.top + line.height >= page.height - margin
}

/// Group fragments that share a baseline into one visual line, joining
/// fragment gaps with a space when they are visually separated.
fn merge_fragments_into_lines(page: &Page) -> Vec<Line> {
    // poppler reports rotated text (arXiv margin watermarks, sidebar
    // decorations) with zero width. It is not part of the reading flow
    // and, left in, it skews the column-midline estimate.
    let mut fragments: Vec<&Fragment> = page
        .fragments
        .iter()
        .filter(|fragment| fragment.width > 0)
        .collect();
    fragments.sort_by_key(|fragment| (fragment.top, fragment.left));

    let mut lines: Vec<Line> = Vec::new();
    for fragment in fragments {
        let size = page
            .font_sizes
            .get(&fragment.font)
            .copied()
            .unwrap_or(fragment.height.unsigned_abs());
        let same_line = lines.last().is_some_and(|line| {
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
            let line = lines.last_mut().expect("checked above");
            let gap = fragment.left - line.right;
            if gap > 1 && !line.spans.last().is_some_and(|s| s.text.ends_with(' ')) {
                push_joined(&mut line.spans, " ", false, false);
            }
            for span in &fragment.spans {
                push_joined(&mut line.spans, &span.text, span.bold, span.italic);
            }
            line.right = line.right.max(fragment.right());
            line.height = line.height.max(fragment.height);
            line.font_size = line.font_size.max(size);
        } else {
            lines.push(Line {
                top: fragment.top,
                left: fragment.left,
                right: fragment.right(),
                height: fragment.height,
                font_size: size,
                spans: fragment.spans.clone(),
            });
        }
    }

    lines.retain(|line| !line.text().trim().is_empty());
    lines
}

fn push_joined(spans: &mut Vec<Span>, text: &str, bold: bool, italic: bool) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = spans.last_mut()
        && last.bold == bold
        && last.italic == italic
    {
        last.text.push_str(text);
        return;
    }
    spans.push(Span {
        text: text.to_string(),
        bold,
        italic,
    });
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
fn heading_levels(pages: &[Page], body_size: u32) -> HashMap<u32, u8> {
    let mut sizes: Vec<u32> = pages
        .iter()
        .flat_map(|page| {
            page.fragments.iter().map(move |fragment| {
                page.font_sizes
                    .get(&fragment.font)
                    .copied()
                    .unwrap_or_else(|| fragment.height.unsigned_abs())
            })
        })
        .filter(|size| *size as f32 >= body_size as f32 * 1.15 && *size >= body_size + 2)
        .collect();
    sizes.sort_unstable_by(|a, b| b.cmp(a));
    sizes.dedup();
    sizes
        .into_iter()
        .take(3)
        .enumerate()
        .map(|(rank, size)| (size, rank as u8 + 1))
        .collect()
}

/// Cluster ordered lines into paragraphs and headings.
fn cluster_paragraphs(
    lines: &[Line],
    body_size: u32,
    heading_levels: &HashMap<u32, u8>,
) -> Vec<PendingBlock> {
    let median_gap = median_line_gap(lines);
    let mut blocks = Vec::new();
    let mut current: Vec<Span> = Vec::new();
    let mut current_heading: Option<u8> = None;
    let mut current_top: Option<i32> = None;
    let mut current_left: Option<i32> = None;
    let mut current_right: Option<i32> = None;
    let mut previous: Option<&Line> = None;

    let flush = |spans: &mut Vec<Span>,
                 heading: &mut Option<u8>,
                 top: &mut Option<i32>,
                 left: &mut Option<i32>,
                 right: &mut Option<i32>,
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
        });
    };

    for line in lines {
        let heading = heading_levels.get(&line.font_size).copied();
        let new_block = match previous {
            None => true,
            Some(prev) => {
                let gap = line.top - prev.top;
                let size_changed = line.font_size != prev.font_size
                    && (heading.is_some()
                        || heading_levels.contains_key(&prev.font_size)
                        || line.font_size.abs_diff(prev.font_size) >= 2);
                let large_gap = gap > (median_gap as f32 * 1.8) as i32 || gap < 0;
                let indented =
                    line.left > prev.left + (body_size as i32) / 2 && prev.left <= line.left; // fresh indent, not a continuation of one
                size_changed || large_gap || indented
            }
        };

        if new_block {
            flush(
                &mut current,
                &mut current_heading,
                &mut current_top,
                &mut current_left,
                &mut current_right,
                &mut blocks,
            );
            current_heading = heading;
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

/// CJK ideographs, kana and hangul syllables have no case; treat them
/// as eligible continuations for the mechanical cross-page paragraph
/// merge when the previous text does not end in any sentence terminal.
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
/// sentence ending in 。！？； is never merged into the next page.
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
        push_joined(buffer, &span.text, span.bold, span.italic);
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
    page: u32,
    mut incoming: Vec<PendingBlock>,
    chapter_prefix: Option<&str>,
) {
    if let (
        Some(AnchoredBlock {
            block: DocBlock::Paragraph { spans: tail },
            ..
        }),
        Some(PendingBlock {
            block: DocBlock::Paragraph { spans: head },
            ..
        }),
    ) = (blocks.last_mut(), incoming.first_mut())
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
            for span in head.drain(..) {
                push_joined(tail, &span.text, span.bold, span.italic);
            }
            incoming.remove(0);
        }
    }
    for anchored in incoming {
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
