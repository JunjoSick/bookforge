use super::*;

pub(super) fn mark_low_confidence_pages(
    page_stats: &mut [PageStats],
    mode: LowConfidenceMode,
) -> Vec<u32> {
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
    if stats.baseline_chars == 0 {
        return false;
    }
    // Judge coverage against the PRE-header-removal character count:
    // running headers/footers repeat on nearly every page and pdftotext
    // includes them in the baseline, so charging their removal against
    // reconstruction quality pushed legitimate pages below the 95%
    // threshold and triggered spurious rasterization/OCR spend
    // (docs/report.md §4.5 PDF-6).
    let credited_chars = stats.chars + stats.running_header_chars;
    (credited_chars as f64 / stats.baseline_chars as f64) < LOW_CONFIDENCE_COVERAGE_RATIO
}

fn low_confidence_action(mode: LowConfidenceMode) -> &'static str {
    match mode {
        LowConfidenceMode::Preserve => "preserve",
        LowConfidenceMode::Linearize => "linearize",
    }
}

pub(super) fn media_layout_warnings(blocks: &[AnchoredBlock]) -> Vec<String> {
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

pub(super) fn baseline_page_char_counts(text: &str, pages: usize) -> Vec<usize> {
    let mut counts = text
        .split('\x0c')
        .map(crate::model::count_visible_chars)
        .collect::<Vec<_>>();
    while counts.last() == Some(&0) && counts.len() > pages {
        counts.pop();
    }
    counts.resize(pages, 0);
    counts
}
