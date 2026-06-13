//! Deterministic layout reconstruction: fragments → lines → columns →
//! reading order → paragraphs/headings.
//!
//! The heuristics here are deliberately simple and inspectable. They are
//! tuned for the two layouts that matter first (ROADMAP §9b.1):
//! single-column books and two-column scientific papers. Anything the
//! heuristics get wrong shows up in the conversion report as a per-page
//! coverage gap, never as silently dropped text.

use std::collections::HashMap;

use crate::model::{ColumnMode, DocBlock, Fragment, Line, Page, Span};

/// Per-page reconstruction diagnostics for the conversion report.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PageStats {
    pub page: u32,
    pub lines: usize,
    pub chars: usize,
    pub two_column: bool,
}

pub struct Reconstruction {
    pub blocks: Vec<DocBlock>,
    pub pages: Vec<PageStats>,
}

pub fn reconstruct(pages: &[Page], columns: ColumnMode) -> Reconstruction {
    let body_size = body_font_size(pages);
    let heading_levels = heading_levels(pages, body_size);

    let mut blocks: Vec<DocBlock> = Vec::new();
    let mut stats = Vec::new();

    for page in pages {
        let lines = merge_fragments_into_lines(page);
        let two_column = match columns {
            ColumnMode::Single => false,
            ColumnMode::Two => true,
            ColumnMode::Auto => detect_two_columns(page, &lines),
        };
        let ordered = if two_column {
            order_two_column(page, &lines)
        } else {
            let mut ordered = lines.clone();
            ordered.sort_by_key(|line| (line.top, line.left));
            ordered
        };

        stats.push(PageStats {
            page: page.number,
            lines: ordered.len(),
            chars: ordered.iter().map(Line::char_count).sum(),
            two_column,
        });

        let page_blocks = cluster_paragraphs(&ordered, body_size, &heading_levels);
        append_with_continuation(&mut blocks, page_blocks);
    }

    Reconstruction {
        blocks,
        pages: stats,
    }
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
fn heading_levels(pages: &[Page], body_size: u32) -> HashMap<u32, u8> {
    let mut sizes: Vec<u32> = pages
        .iter()
        .flat_map(|page| page.font_sizes.values().copied())
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
) -> Vec<DocBlock> {
    let median_gap = median_line_gap(lines);
    let mut blocks = Vec::new();
    let mut current: Vec<Span> = Vec::new();
    let mut current_heading: Option<u8> = None;
    let mut previous: Option<&Line> = None;

    let flush = |spans: &mut Vec<Span>, heading: &mut Option<u8>, blocks: &mut Vec<DocBlock>| {
        if spans.is_empty() {
            return;
        }
        let spans = std::mem::take(spans);
        match heading.take() {
            Some(level) => blocks.push(DocBlock::Heading { level, spans }),
            None => blocks.push(DocBlock::Paragraph { spans }),
        }
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
            flush(&mut current, &mut current_heading, &mut blocks);
            current_heading = heading;
        }
        join_line_into(&mut current, line);
        previous = Some(line);
    }
    flush(&mut current, &mut current_heading, &mut blocks);

    blocks
}

/// Append a line's spans to a paragraph buffer, repairing soft hyphens
/// at the join.
fn join_line_into(buffer: &mut Vec<Span>, line: &Line) {
    if buffer.is_empty() {
        buffer.extend(line.spans.iter().cloned());
        return;
    }

    let next_starts_lower = line
        .text()
        .chars()
        .next()
        .is_some_and(|ch| ch.is_lowercase());
    let hyphenated = buffer
        .last()
        .is_some_and(|span| span.text.trim_end().ends_with('-'));

    if hyphenated && next_starts_lower {
        if let Some(last) = buffer.last_mut() {
            let trimmed = last.text.trim_end();
            let without = trimmed[..trimmed.len() - 1].to_string();
            last.text = without;
        }
    } else if !buffer.last().is_some_and(|span| span.text.ends_with(' ')) {
        push_joined(buffer, " ", false, false);
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
/// sentence and the new text starts lowercase.
fn append_with_continuation(blocks: &mut Vec<DocBlock>, mut incoming: Vec<DocBlock>) {
    if let (Some(DocBlock::Paragraph { spans: tail }), Some(DocBlock::Paragraph { spans: head })) =
        (blocks.last_mut(), incoming.first_mut())
    {
        let tail_text: String = tail.iter().map(|span| span.text.as_str()).collect();
        let head_text: String = head.iter().map(|span| span.text.as_str()).collect();
        let continues = !tail_text
            .trim_end()
            .ends_with(['.', '!', '?', ':', ';', '"', '\u{201d}', '\u{2019}'])
            && head_text.chars().next().is_some_and(|ch| ch.is_lowercase());
        if continues {
            let hyphenated = tail_text.trim_end().ends_with('-');
            if hyphenated {
                if let Some(last) = tail.last_mut() {
                    let trimmed = last.text.trim_end();
                    last.text = trimmed[..trimmed.len() - 1].to_string();
                }
            } else if !tail.last().is_some_and(|span| span.text.ends_with(' ')) {
                push_joined(tail, " ", false, false);
            }
            for span in head.drain(..) {
                push_joined(tail, &span.text, span.bold, span.italic);
            }
            incoming.remove(0);
        }
    }
    blocks.append(&mut incoming);
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
        let texts: Vec<String> = result.blocks.iter().map(DocBlock::text).collect();

        assert!(
            matches!(&result.blocks[0], DocBlock::Heading { level: 1, .. }),
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

        let texts: Vec<String> = result.blocks.iter().map(DocBlock::text).collect();
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
            .map(|block| block.text())
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

        let texts: Vec<String> = result.blocks.iter().map(DocBlock::text).collect();
        assert_eq!(
            texts,
            vec!["This sentence does not end until the following page.".to_string()]
        );
    }
}
