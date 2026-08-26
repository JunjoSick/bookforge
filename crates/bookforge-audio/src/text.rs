//! Turning a parsed [`Book`] into narratable chapter text and splitting
//! that text into synthesis-sized chunks.
//!
//! TTS endpoints cap the number of characters per request (OpenAI's is
//! 4096). We keep chunks well under that and, crucially, cut on sentence
//! boundaries so the narrator's prosody resets at a natural pause instead
//! of mid-clause. Chunking is a pure function of the text, so a resumed run
//! re-derives exactly the same chunk list and can skip already-rendered
//! files by name.

use bookforge_core::ir::{Block, BlockId, BlockKind, Book};
use bookforge_core::marker::strip_marker_tokens;
use bookforge_core::script::is_space_delimited;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NarrationBlockKind {
    Title,
    Heading(u8),
    Paragraph,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NarrationBlock {
    pub kind: NarrationBlockKind,
    pub text: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChunkKind {
    Title,
    Heading,
    #[default]
    Body,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NarrationChunk {
    pub kind: ChunkKind,
    pub text: String,
}

/// One narratable chapter: the visible prose of a single spine section,
/// with a display title used for filenames and (when stitched) chapter
/// markers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chapter {
    /// Zero-based position in reading order.
    pub index: usize,
    pub title: String,
    /// Clean narratable blocks with inline markers removed. Empty for
    /// sections that carry no readable text (cover images, nav documents).
    pub blocks: Vec<NarrationBlock>,
}

impl Chapter {
    pub fn text(&self) -> String {
        self.blocks
            .iter()
            .map(|block| block.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.iter().all(|block| block.text.trim().is_empty())
    }
}

/// Extract narratable chapters in reading order.
///
/// The reader synthesizes structural sections from the OPF metadata and the
/// NCX table of contents so those get translated; narration does not want
/// them (nobody wants a table of contents read aloud), so they are skipped
/// here. Chapters are numbered contiguously over the sections that survive
/// filtering. Sections with no readable prose are still returned (with empty
/// `text`) so the builder can skip them via [`Chapter::is_empty`].
pub fn chapters_from_book(book: &Book) -> Vec<Chapter> {
    chapters_from_book_with_options(book, false)
}

/// Extract chapters with optional physical-page grouping for positively
/// identified pdftohtml sources. Ordinary EPUBs always preserve their spine
/// section boundaries.
pub fn chapters_from_book_with_options(book: &Book, pdf_page_grouping: bool) -> Vec<Chapter> {
    let block_index: std::collections::HashMap<&BlockId, &Block> =
        book.blocks.iter().map(|block| (&block.id, block)).collect();
    let navigation_hrefs: std::collections::HashSet<String> = book
        .manifest
        .iter()
        .filter(|resource| resource.properties.iter().any(|value| value == "nav"))
        .map(|resource| normalized_href(&resource.href))
        .collect();

    let sections = book
        .sections
        .iter()
        .filter(|section| {
            !section.id.0.starts_with("sec_nav_")
                && is_narratable_section(&section.href, &navigation_hrefs)
                && !looks_like_table_of_contents(section, &block_index)
        })
        .collect::<Vec<_>>();

    // Some PDF converters emit one XHTML spine item per physical page. In
    // that shape, treating every spine item as a chapter narrates folios and
    // creates hundreds of meaningless chapter markers. Keep ordinary EPUBs
    // strictly section-preserving, but fold clearly page-sliced books around
    // explicit localized chapter headings.
    let numeric_titles = sections
        .iter()
        .filter(|section| section.title.as_deref().is_some_and(is_ascii_folio))
        .count();
    if (sections.len() >= 8 && numeric_titles * 2 >= sections.len())
        || (pdf_page_grouping && sections.len() >= 8)
    {
        return chapters_from_pdf_pages(book, &sections, &block_index);
    }

    sections
        .into_iter()
        .enumerate()
        .map(|(index, section)| {
            let mut blocks = Vec::new();
            let mut first_heading = true;
            for block_id in &section.block_ids {
                let Some(block) = block_index.get(block_id) else {
                    continue;
                };
                if block.kind == BlockKind::PageFurniture {
                    continue;
                }
                let text = clean_block_text(block);
                if !text.is_empty() {
                    let kind = match block.kind {
                        BlockKind::Heading(_) if first_heading => {
                            first_heading = false;
                            NarrationBlockKind::Title
                        }
                        BlockKind::Heading(level) => NarrationBlockKind::Heading(level),
                        _ => NarrationBlockKind::Paragraph,
                    };
                    blocks.push(NarrationBlock { kind, text });
                }
            }
            let title = section
                .title
                .as_ref()
                .map(|title| title.trim().to_string())
                .filter(|title| !title.is_empty())
                .unwrap_or_else(|| format!("Chapter {}", index + 1));
            Chapter {
                index,
                title,
                blocks,
            }
        })
        .collect()
}

fn chapters_from_pdf_pages(
    book: &Book,
    sections: &[&bookforge_core::ir::Section],
    block_index: &std::collections::HashMap<&BlockId, &Block>,
) -> Vec<Chapter> {
    let mut occurrences = std::collections::HashMap::<String, usize>::new();
    for section in sections {
        let mut seen_on_page = std::collections::HashSet::new();
        for block_id in &section.block_ids {
            let Some(block) = block_index.get(block_id) else {
                continue;
            };
            let text = clean_block_text(block);
            if is_repeated_furniture_candidate(&text) {
                seen_on_page.insert(text.to_lowercase());
            }
        }
        for text in seen_on_page {
            *occurrences.entry(text).or_default() += 1;
        }
    }

    let mut chapters = Vec::new();
    let mut title = book
        .metadata
        .title
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "Front matter".to_string());
    let mut blocks = Vec::<NarrationBlock>::new();
    let mut printed_toc = false;
    let mut printed_toc_roman_folio = false;

    for section in sections {
        let mut page_start = blocks.len();
        for block_id in &section.block_ids {
            let Some(block) = block_index.get(block_id) else {
                continue;
            };
            if block.kind == BlockKind::PageFurniture {
                continue;
            }
            let mut text = clean_block_text(block);
            if text.is_empty() || is_ascii_folio(&text) {
                continue;
            }

            // In PDF-derived books the printed TOC is often followed by
            // Roman-numbered front-matter pages. The first prose after that
            // folio belongs to the book (for example an author disclaimer),
            // even when it precedes a conventional Introduction heading.
            if printed_toc && is_roman_folio(&text) {
                printed_toc_roman_folio = true;
                continue;
            }

            if let Some(offset) = embedded_chapter_label_offset(&text) {
                text = text[offset..].trim().to_string();
            }

            if chapter_label_key(&text).is_some() {
                printed_toc = false;
                printed_toc_roman_folio = false;
                if !blocks.is_empty() {
                    chapters.push(Chapter {
                        index: chapters.len(),
                        title,
                        blocks,
                    });
                    blocks = Vec::new();
                    page_start = 0;
                }
                title = text.clone();
            } else if chapters.is_empty()
                && (is_printed_toc_heading(&text) || is_printed_toc_entry(&text))
            {
                printed_toc = true;
                printed_toc_roman_folio = false;
                continue;
            } else if printed_toc && (is_front_matter_heading(&text) || printed_toc_roman_folio) {
                printed_toc = false;
                printed_toc_roman_folio = false;
            } else if printed_toc
                || occurrences
                    .get(&text.to_lowercase())
                    .is_some_and(|count| *count >= 3)
            {
                continue;
            }
            let kind = if chapter_label_key(&text).is_some() {
                NarrationBlockKind::Title
            } else {
                match block.kind {
                    BlockKind::Heading(level) => NarrationBlockKind::Heading(level),
                    _ => NarrationBlockKind::Paragraph,
                }
            };
            blocks.push(NarrationBlock { kind, text });
        }

        // Join only a paragraph that actually crosses a physical page. Do
        // not flatten ordinary paragraph boundaries within the page.
        if page_start > 0
            && blocks.len() > page_start
            && blocks[page_start - 1].kind == NarrationBlockKind::Paragraph
            && blocks[page_start].kind == NarrationBlockKind::Paragraph
            && should_join_across_page(&blocks[page_start - 1].text, &blocks[page_start].text)
        {
            let right = blocks.remove(page_start).text;
            blocks[page_start - 1].text.push(' ');
            blocks[page_start - 1].text.push_str(&right);
        }
    }

    if !blocks.is_empty() {
        chapters.push(Chapter {
            index: chapters.len(),
            title,
            blocks,
        });
    }
    chapters
}

fn is_ascii_folio(text: &str) -> bool {
    let trimmed = text.trim();
    !trimmed.is_empty() && trimmed.chars().all(|ch| ch.is_ascii_digit())
}

fn is_roman_folio(text: &str) -> bool {
    let trimmed = text.trim();
    !trimmed.is_empty()
        && trimmed.len() <= 8
        && trimmed
            .chars()
            .all(|ch| matches!(ch.to_ascii_uppercase(), 'I' | 'V' | 'X' | 'L' | 'C'))
}

fn is_printed_toc_heading(text: &str) -> bool {
    matches!(
        text.trim().to_lowercase().as_str(),
        "indice" | "índice" | "contents" | "table of contents" | "sommaire" | "inhaltsverzeichnis"
    )
}

fn is_printed_toc_entry(text: &str) -> bool {
    let lower = text.trim().to_lowercase();
    let chapter_refs = lower.matches("cap.").count();
    let page_refs = lower.matches("pag.").count();
    let first_number = lower
        .strip_prefix("cap.")
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or_default()
        .trim_matches(|ch: char| !ch.is_ascii_alphanumeric());
    let roman_or_digits = !first_number.is_empty()
        && first_number.chars().all(|ch| {
            ch.is_ascii_digit() || matches!(ch.to_ascii_uppercase(), 'I' | 'V' | 'X' | 'L' | 'C')
        });
    (chapter_refs >= 2 && page_refs >= 2)
        || (lower.starts_with("cap.") && (lower.contains("pag.") || roman_or_digits))
}

fn is_front_matter_heading(text: &str) -> bool {
    matches!(
        text.trim().to_lowercase().as_str(),
        "nota dell’autore"
            | "nota dell'autore"
            | "introduzione"
            | "introduction"
            | "preface"
            | "foreword"
            | "avant-propos"
            | "vorwort"
            | "prólogo"
            | "prologo"
    )
}

fn is_repeated_furniture_candidate(text: &str) -> bool {
    let chars = text.chars().count();
    chars > 0 && chars <= 80 && chapter_label_key(text).is_none()
}

fn chapter_label_key(text: &str) -> Option<String> {
    const LABELS: &[&str] = &[
        "chapter",
        "capitolo",
        "chapitre",
        "kapitel",
        "capítulo",
        "capitulo",
    ];
    let normalized_words = text
        .split_whitespace()
        .map(|word| word.trim_matches(|ch: char| !ch.is_alphanumeric()))
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    let label = *normalized_words.first()?;

    // Canonical Toki Pona chapter headings keep the protected Roman numeral:
    // `lipu nanpa VI`. Requiring a Roman numeral prevents ordinary phrases
    // such as `lipu nanpa 17 ...` from becoming accidental chapter breaks.
    if normalized_words.len() >= 3
        && label.eq_ignore_ascii_case("lipu")
        && normalized_words[1].eq_ignore_ascii_case("nanpa")
        && is_roman_number(normalized_words[2])
    {
        return Some(format!(
            "lipu nanpa {}",
            normalized_words[2].to_ascii_uppercase()
        ));
    }

    // Early BookForge Toki Pona output used the non-standard KAPITELO label.
    // Recognize it when narrating existing files, but translation prompts and
    // validation continue to require ordinary `lipu nanpa <Roman>`.
    if label.eq_ignore_ascii_case("kapitelo")
        && normalized_words.len() >= 2
        && normalized_words.len() <= 5
        && normalized_words[1..].iter().all(|word| {
            is_roman_number(word)
                || matches!(
                    word.to_ascii_lowercase().as_str(),
                    "wan" | "tu" | "luka" | "mute" | "ale"
                )
        })
    {
        return Some(
            normalized_words
                .iter()
                .map(|word| word.to_ascii_uppercase())
                .collect::<Vec<_>>()
                .join(" "),
        );
    }

    if !LABELS.iter().any(|known| label.eq_ignore_ascii_case(known)) {
        return None;
    }
    let number = *normalized_words.get(1)?;
    let roman = is_roman_number(number);
    if !number.chars().all(|ch| ch.is_ascii_digit()) && !roman {
        return None;
    }
    Some(format!(
        "{} {}",
        label.to_ascii_lowercase(),
        number.to_ascii_uppercase()
    ))
}

fn is_roman_number(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|ch| matches!(ch.to_ascii_uppercase(), 'I' | 'V' | 'X' | 'L' | 'C'))
}

fn embedded_chapter_label_offset(text: &str) -> Option<usize> {
    for (offset, _) in text.char_indices().skip(1) {
        if offset > 80 || !text[..offset].ends_with(char::is_whitespace) {
            continue;
        }
        let suffix = &text[offset..];
        let label = suffix
            .split_whitespace()
            .next()?
            .trim_matches(|ch: char| !ch.is_alphabetic());
        if label.chars().any(char::is_lowercase) {
            continue;
        }
        if chapter_label_key(suffix).is_some() {
            return Some(offset);
        }
    }
    None
}

fn should_join_across_page(left: &str, right: &str) -> bool {
    let left_ends_sentence = left
        .trim_end_matches(['\"', '\'', '”', '’', ')', ']'])
        .ends_with(['.', '!', '?', ':', ';']);
    let right_starts_lowercase = right
        .chars()
        .find(|ch| ch.is_alphabetic())
        .is_some_and(char::is_lowercase);
    !left_ends_sentence && right_starts_lowercase
}

/// A section is narratable unless it comes from a structural package or
/// navigation resource. EPUB 3 navigation documents are XHTML, so checking
/// extensions alone would incorrectly read the table of contents aloud.
fn is_narratable_section(href: &str, navigation_hrefs: &std::collections::HashSet<String>) -> bool {
    let path = normalized_href(href);
    let is_navigation = navigation_hrefs.iter().any(|navigation| {
        path == *navigation
            || path
                .strip_suffix(navigation)
                .is_some_and(|prefix| prefix.ends_with('/'))
    });
    !(path.ends_with(".opf") || path.ends_with(".ncx") || is_navigation)
}

/// nav-audio residual backstop: a malformed EPUB 3 whose navigation document
/// forgot the `nav` property still declares itself through its file name.
/// True when the stem looks like a ToC container (`nav.xhtml`, `toc-1.html`,
/// `OEBPS/contents.xhtml`, …). Structural sections (opf/ncx) are filtered
/// before this runs; the match is deliberately narrow to avoid dropping real
/// chapters whose author liked the word "navigation".
fn toc_named_href(path: &str) -> bool {
    if !matches!(path.rsplit('.').next(), Some("xhtml" | "html" | "htm")) {
        return false;
    }
    let Some(file) = path.rsplit('/').next() else {
        return false;
    };
    let Some((stem, _)) = file.split_once('.') else {
        return false;
    };
    let stem = stem.trim_matches(['-', '_', ' ', '.']);
    for prefix in [
        "table of contents",
        "table-of-contents",
        "toc",
        "nav",
        "contents",
    ] {
        if stem == prefix
            || stem
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with(['-', '_', ' ']))
        {
            return true;
        }
    }
    false
}

/// Heuristic backstop for EPUBs whose printed/navigation ToC survives in a
/// regular spine document without any `nav` property to identify it. Such a
/// section would otherwise be narrated wholesale ("Chapter Three dot dot dot
/// one hundred and twenty seven"). A spine section counts as a ToC when its
/// readable blocks overwhelmingly look like entries: short lines, most ending
/// in an Arabic or Roman folio. Real prose chapters never approach that ratio,
/// while lists of illustrations/maps — furniture under every reading — do.
///
/// text_coverage honesty is unaffected: coverage is computed over the source
/// document by the epub crate, not over what narration skips.
fn looks_like_table_of_contents(
    section: &bookforge_core::ir::Section,
    block_index: &std::collections::HashMap<&BlockId, &Block>,
) -> bool {
    if toc_named_href(&normalized_href(&section.href)) {
        return true;
    }
    let mut blocks = 0usize;
    let mut folio_terminated = 0usize;
    let mut total_chars = 0usize;
    for block_id in &section.block_ids {
        let Some(block) = block_index.get(block_id) else {
            continue;
        };
        if block.kind == BlockKind::PageFurniture {
            continue;
        }
        let text = clean_block_text(block);
        if text.is_empty() {
            continue;
        }
        blocks += 1;
        total_chars += text.chars().count();
        if text.ends_with_folio() {
            folio_terminated += 1;
        }
    }
    blocks >= 8
        && folio_terminated * 4 >= blocks * 3
        && total_chars / blocks <= TOC_ENTRY_MAX_AVG_CHARS
}

const TOC_ENTRY_MAX_AVG_CHARS: usize = 60;

trait EndsWithFolio {
    fn ends_with_folio(&self) -> bool;
}

impl EndsWithFolio for str {
    fn ends_with_folio(&self) -> bool {
        let trimmed = self.trim_end_matches(['.', ')', ']']);
        if trimmed.is_empty() {
            return false;
        }
        if trimmed
            .bytes()
            .last()
            .is_some_and(|byte| byte.is_ascii_digit())
        {
            return true;
        }
        is_roman_folio(
            trimmed
                .rsplit(char::is_whitespace)
                .next()
                .unwrap_or_default(),
        )
    }
}

fn normalized_href(href: &str) -> String {
    href.split(['#', '?'])
        .next()
        .unwrap_or(href)
        .replace('\\', "/")
        .to_ascii_lowercase()
}

/// Join a block's text runs, drop inline marker tokens, and collapse
/// whitespace so the narration reads cleanly.
fn clean_block_text(block: &Block) -> String {
    let joined: String = block
        .text_runs
        .iter()
        .map(|run| run.text.as_str())
        .collect();
    let stripped = strip_marker_tokens(&joined);
    collapse_whitespace(&stripped)
}

fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(ch);
        }
    }
    out
}

/// Split chapter prose into chunks no longer than `max_chars` characters,
/// cutting on sentence boundaries wherever possible. A sentence longer than
/// the limit on its own is split at word boundaries, and a single word
/// longer than the limit is split at a character boundary as a last resort.
///
/// `max_chars` counts Unicode scalar values, not bytes; every returned
/// chunk is a valid string and non-empty. Returns an empty vector for
/// blank input.
pub fn chunk_blocks(blocks: &[NarrationBlock], max_chars: usize) -> Vec<NarrationChunk> {
    let max_chars = max_chars.max(1);
    let mut chunks = Vec::new();
    let mut paragraphs = Vec::new();

    let flush_paragraphs =
        |paragraphs: &mut Vec<&str>, chunks: &mut Vec<NarrationChunk>| {
            if paragraphs.is_empty() {
                return;
            }
            let text = paragraphs.join("\n\n");
            chunks.extend(chunk_body_text(&text, max_chars).into_iter().map(|text| {
                NarrationChunk {
                    kind: ChunkKind::Body,
                    text,
                }
            }));
            paragraphs.clear();
        };

    for block in blocks {
        match block.kind {
            NarrationBlockKind::Paragraph => paragraphs.push(block.text.as_str()),
            NarrationBlockKind::Title | NarrationBlockKind::Heading(_) => {
                flush_paragraphs(&mut paragraphs, &mut chunks);
                let kind = match block.kind {
                    NarrationBlockKind::Title => ChunkKind::Title,
                    NarrationBlockKind::Heading(_) => ChunkKind::Heading,
                    NarrationBlockKind::Paragraph => unreachable!(),
                };
                let text = block.text.trim();
                if !text.is_empty() {
                    let pieces = if text.chars().count() > max_chars {
                        split_long_unit(text, max_chars)
                    } else {
                        vec![text.to_string()]
                    };
                    chunks.extend(pieces.into_iter().map(|text| NarrationChunk { kind, text }));
                }
            }
        }
    }
    flush_paragraphs(&mut paragraphs, &mut chunks);
    chunks
}

/// Compatibility wrapper for callers that have unstructured prose.
pub fn chunk_text(text: &str, max_chars: usize) -> Vec<String> {
    chunk_blocks(
        &[NarrationBlock {
            kind: NarrationBlockKind::Paragraph,
            text: text.to_string(),
        }],
        max_chars,
    )
    .into_iter()
    .map(|chunk| chunk.text)
    .collect()
}

fn chunk_body_text(text: &str, max_chars: usize) -> Vec<String> {
    let max_chars = max_chars.max(1);
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize;

    for sentence in split_sentences(text) {
        let sentence_len = sentence.chars().count();

        if sentence_len > max_chars {
            // Flush whatever we have, then hard-split the long sentence.
            if current_len > 0 {
                chunks.push(std::mem::take(&mut current));
                current_len = 0;
            }
            for piece in split_long_unit(&sentence, max_chars) {
                chunks.push(piece);
            }
            continue;
        }

        let separator = usize::from(current_len > 0);
        if current_len + separator + sentence_len > max_chars {
            chunks.push(std::mem::take(&mut current));
            current_len = 0;
        }

        if current_len > 0 {
            current.push(' ');
            current_len += 1;
        }
        current.push_str(&sentence);
        current_len += sentence_len;
    }

    if current_len > 0 {
        chunks.push(current);
    }

    chunks
}

/// Break text into trimmed sentences. Sentence terminators are `.`, `!`,
/// `?`, `…` and the CJK terminators `。`, `！`, `？`. A Latin terminator is
/// only taken when followed by whitespace or end of input, and common
/// abbreviations ("Mr.", "e.g.", single-letter initials) never break, which
/// keeps decimals like "3.14" intact too. CJK terminators always break —
/// CJK prose has no spaces to fall back on, so requiring one would swallow
/// whole paragraphs into a single sentence. Closing quotes and brackets that
/// immediately follow a terminator stay attached to their sentence, so
/// `他说。"…"`-style punctuation is not orphaned. Blank lines also force a
/// boundary so paragraph structure is respected.
fn split_sentences(text: &str) -> Vec<String> {
    let mut sentences: Vec<String> = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0usize;

    while index < chars.len() {
        let ch = chars[index];
        current.push(ch);
        if is_sentence_terminator(ch) && terminator_ends_sentence(&chars, index, &current) {
            index += 1;
            // Attach any run of closing quotes/brackets plus an ellipsis run
            // tail to the finished sentence; narrating a lone """ or "…" is
            // what produced the old orphaned-punctuation artifacts.
            while index < chars.len() && is_closing_punctuation(chars[index]) {
                current.push(chars[index]);
                index += 1;
            }
            while index < chars.len() && chars[index] == '…' {
                current.push(chars[index]);
                index += 1;
                if !chars.get(index).is_some_and(|&next| next == '…') {
                    break;
                }
            }
            push_trimmed(&mut sentences, &mut current);
            continue;
        }
        if ch == '\n' {
            // A paragraph break (blank line) is a hard boundary even without
            // terminal punctuation, e.g. headings.
            if chars.get(index + 1).is_none_or(|next| *next == '\n') {
                push_trimmed(&mut sentences, &mut current);
            }
        }
        index += 1;
    }
    push_trimmed(&mut sentences, &mut current);
    sentences
}

fn is_sentence_terminator(ch: char) -> bool {
    matches!(ch, '.' | '!' | '?' | '…' | '。' | '！' | '？')
}

/// CJK terminators carry no whitespace convention, so they always end a
/// sentence; Latin-script terminators keep the historical whitespace rule and
/// additionally suppress breaks inside abbreviations and initials.
fn terminator_ends_sentence(chars: &[char], index: usize, current: &str) -> bool {
    match chars[index] {
        '。' | '！' | '？' => true,
        '…' => true,
        _ => following_is_break(chars, index) && !is_abbreviation_boundary(current),
    }
}

fn following_is_break(chars: &[char], index: usize) -> bool {
    chars
        .get(index + 1)
        .map(|next| next.is_whitespace())
        .unwrap_or(true)
}

fn is_closing_punctuation(ch: char) -> bool {
    matches!(
        ch,
        '"' | '\'' | '”' | '’' | ')' | ']' | '」' | '』' | '）' | '】' | '》'
    )
}

const SENTENCE_ABBREVIATIONS: &[&str] = &[
    "mr", "mrs", "ms", "dr", "prof", "sr", "jr", "st", "mt", "vs", "etc", "eg", "ie", "cf", "al",
    "fig", "no", "vol", "pp", "ed", "approx", "dept", "est", "inc", "ltd", "capt", "sgt", "lt",
];

/// True when `current` ends in a form whose final dot is part of the word:
/// "Mr.", "e.g.", "i.e.", "Inc." — or a person initial such as "J.".
/// Everything is compared on the trailing alphanumerics before the dot, so
/// entity-escaped or full-width look-alikes are untouched.
fn is_abbreviation_boundary(current: &str) -> bool {
    let Some(dot_position) = current
        .char_indices()
        .rev()
        .find(|(_, ch)| *ch == '.')
        .map(|(i, _)| i)
    else {
        return false;
    };
    if dot_position == 0 {
        return false;
    }
    let token: String = current[..dot_position]
        .chars()
        .rev()
        .take_while(|ch| ch.is_alphanumeric())
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>()
        .to_ascii_lowercase();
    if token.is_empty() {
        return false;
    }
    // A lone letter before the dot reads as an initial ("J. R. R. Tolkien");
    // lower-case single letters after dots are still rare enough in prose
    // that keeping them unbroken is harmless.
    if token.chars().count() == 1 {
        return true;
    }
    SENTENCE_ABBREVIATIONS.contains(&token.as_str())
}

fn push_trimmed(sentences: &mut Vec<String>, current: &mut String) {
    let trimmed = current.trim();
    if !trimmed.is_empty() {
        sentences.push(trimmed.to_string());
    }
    current.clear();
}

/// Split a unit longer than `max_chars`, preferring word boundaries and
/// falling back to character boundaries for a single oversize word.
///
/// Unspaced scripts (Han, Kana, Hangul — anything failing
/// [`is_space_delimited`]) have no words to fall back on, so plain
/// whitespace splitting degenerates to counting characters mid-word. For
/// those units the splitter first cuts on CJK clause punctuation (、 ， ； ：),
/// which yields natural narratable clauses; only a clause still longer than
/// the limit ends up hard-split on a character boundary, which remains
/// Unicode-scalar safe.
fn split_long_unit(unit: &str, max_chars: usize) -> Vec<String> {
    let mut pieces: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize;

    // Unspaced units are rejoined without separators so the chunks
    // reproduce the source bytes exactly; whitespace-delimited ones keep the
    // historical single-space joins.
    let (words, separator) = if is_space_delimited(unit) {
        (
            unit.split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>(),
            Some(' '),
        )
    } else {
        (split_unspaced_clauses(unit), None)
    };

    for word in &words {
        let word_len = word.chars().count();
        if word_len > max_chars {
            if current_len > 0 {
                pieces.push(std::mem::take(&mut current));
                current_len = 0;
            }
            pieces.extend(split_by_chars(word, max_chars));
            continue;
        }
        let separator_len = usize::from(separator.is_some() && current_len > 0);
        if current_len + separator_len + word_len > max_chars {
            pieces.push(std::mem::take(&mut current));
            current_len = 0;
        }
        if current_len > 0
            && let Some(sep) = separator
        {
            current.push(sep);
            current_len += 1;
        }
        current.push_str(word);
        current_len += word_len;
    }

    if current_len > 0 {
        pieces.push(current);
    }
    pieces
}

/// Cut unspaced prose into clause-sized units at ideographic commas,
/// semicolons, and colons (plus the ASCII comma for mixed inserts). The
/// punctuation stays attached to its clause so re-joining with spaces
/// preserves every character exactly. A piece still longer than the chunk
/// limit falls through to the hard character-boundary split upstream.
fn split_unspaced_clauses(unit: &str) -> Vec<String> {
    const CLAUSE_BREAKS: &[char] = &['、', '，', '；', '：', ','];
    let mut clauses = Vec::new();
    let mut current = String::new();
    for ch in unit.chars() {
        current.push(ch);
        if CLAUSE_BREAKS.contains(&ch) && !current.trim().is_empty() {
            clauses.push(std::mem::take(&mut current));
        }
    }
    if !current.trim().is_empty() {
        clauses.push(current);
    }
    clauses.retain(|clause| !clause.is_empty());
    clauses
}

fn split_by_chars(word: &str, max_chars: usize) -> Vec<String> {
    let mut pieces: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut count = 0usize;
    for ch in word.chars() {
        current.push(ch);
        count += 1;
        if count == max_chars {
            pieces.push(std::mem::take(&mut current));
            count = 0;
        }
    }
    if !current.is_empty() {
        pieces.push(current);
    }
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapses_internal_whitespace() {
        assert_eq!(collapse_whitespace("  a\n\t b  "), "a b");
    }

    #[test]
    fn splits_on_sentence_terminators_followed_by_space() {
        let sentences = split_sentences("One. Two! Three?");
        assert_eq!(sentences, vec!["One.", "Two!", "Three?"]);
    }

    #[test]
    fn does_not_split_decimals_or_abbreviations() {
        // No trailing space after the internal dots, so they are not breaks.
        let sentences = split_sentences("Pi is 3.14 exactly. Done.");
        assert_eq!(sentences, vec!["Pi is 3.14 exactly.", "Done."]);
    }

    #[test]
    fn chunk_groups_whole_sentences_under_limit() {
        let chunks = chunk_text("One. Two. Three.", 9);
        // "One. Two." = 9 chars fits; "Three." starts a new chunk.
        assert_eq!(chunks, vec!["One. Two.", "Three."]);
    }

    #[test]
    fn chunk_splits_oversize_sentence_on_word_boundaries() {
        let chunks = chunk_text("alpha beta gamma delta", 11);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= 11, "chunk too long: {chunk:?}");
        }
        assert_eq!(chunks.join(" "), "alpha beta gamma delta");
    }

    #[test]
    fn chunk_splits_single_oversize_word_on_char_boundary() {
        let chunks = chunk_text("supercalifragilistic", 5);
        assert!(chunks.iter().all(|c| c.chars().count() <= 5));
        assert_eq!(chunks.concat(), "supercalifragilistic");
    }

    #[test]
    fn chunk_is_unicode_safe() {
        // Multi-byte characters must never be split mid-scalar.
        let chunks = chunk_text("日本語のテキストです", 3);
        assert!(chunks.iter().all(|c| c.chars().count() <= 3));
        assert_eq!(chunks.concat(), "日本語のテキストです");
    }

    #[test]
    fn empty_text_yields_no_chunks() {
        assert!(chunk_text("   \n  ", 100).is_empty());
    }

    #[test]
    fn abbreviations_and_initials_do_not_terminate_sentences() {
        assert_eq!(
            split_sentences("Mr. Smith met Dr. Who at 3 p. m. Today was fine."),
            vec!["Mr. Smith met Dr. Who at 3 p. m. Today was fine."]
        );
        assert_eq!(
            split_sentences("Compare e.g. apples vs. pears, i.e. fruit."),
            vec!["Compare e.g. apples vs. pears, i.e. fruit."]
        );
        assert_eq!(
            split_sentences("It was J. R. R. Tolkien himself. He wrote on."),
            vec!["It was J. R. R. Tolkien himself.", "He wrote on."]
        );
    }

    #[test]
    fn cjk_terminators_break_without_whitespace() {
        assert_eq!(
            split_sentences("矛盾是普遍的。矛盾又是特殊的！这是为何？"),
            vec!["矛盾是普遍的。", "矛盾又是特殊的！", "这是为何？"]
        );
        // Ellipsis runs stay attached to their sentence instead of leaking
        // an orphaned "…" into the next chunk.
        assert_eq!(
            split_sentences("他犹豫了……然后开口。"),
            vec!["他犹豫了……", "然后开口。"]
        );
        // Closing punctuation after a CJK terminator stays attached; a lone
        // right quote never becomes its own chunk.
        assert_eq!(
            split_sentences("他说。”然后他沉默了。"),
            vec!["他说。”", "然后他沉默了。"]
        );
    }

    #[test]
    fn unspaced_prose_splits_on_clause_punctuation_not_mid_word() {
        let unit = "矛盾的普遍性和特殊性，是矛盾问题的精髓，一切皆然；这是定论：无人可以反驳";
        let pieces = split_long_unit(unit, 12);
        for piece in &pieces {
            assert!(piece.chars().count() <= 12, "piece too long: {piece:?}");
        }
        // Character-exact round trip: no injected separators, nothing lost.
        let rejoined: String = if pieces.len() > 1 {
            pieces.join("")
        } else {
            pieces.concat()
        };
        assert_eq!(rejoined.replace(' ', ""), unit);
        assert_eq!(rejoined.chars().filter(|ch| *ch == '，').count(), 2);
        // Every break lands right after clause punctuation, never mid-word.
        for window in pieces.windows(2) {
            let boundary = window[0].chars().last().unwrap();
            assert!(
                "，；：、".contains(boundary) || window[0].ends_with('。'),
                "bad boundary before {:?}",
                window[1]
            );
        }
    }

    #[test]
    fn malformed_nav_document_is_excluded_from_narration() {
        use bookforge_core::ir::{
            Block, BlockId, BookFormat, BookId, DomPath, Metadata, Section, SectionId, TextRun,
        };

        let make_section = |id: &str,
                            spine_index: usize,
                            href: &str,
                            entries: Vec<String>,
                            blocks: &mut Vec<Block>| {
            let section_id = SectionId(id.to_string());
            let block_ids = entries
                .into_iter()
                .enumerate()
                .map(|(part, text)| {
                    let block_id = BlockId(format!("{id}-{part}"));
                    blocks.push(Block {
                        id: block_id.clone(),
                        section_id: section_id.clone(),
                        kind: BlockKind::Paragraph,
                        dom_path: DomPath(vec![part]),
                        text_runs: vec![TextRun {
                            id: format!("{id}-r{part}"),
                            text,
                        }],
                        inline_marks: Vec::new(),
                        protected_spans: Vec::new(),
                        token_estimate: 1,
                    });
                    block_id
                })
                .collect();
            Section {
                id: section_id,
                href: href.to_string(),
                spine_index,
                title: None,
                heading_level: None,
                block_ids,
                prev: None,
                next: None,
            }
        };
        let mut blocks = Vec::new();
        let toc_entries = (1..=10)
            .map(|page| format!("Capitolo {page} / La storia Pag. {}", page * 9))
            .collect();
        let prose = (0..10)
            .map(|paragraph| {
                format!("Il capitolo numero {paragraph} narra una vicenda completa di lunga prosa")
            })
            .collect();
        let book = Book {
            source_path: None,
            id: BookId("nav-less".to_string()),
            format: BookFormat::Epub,
            metadata: Metadata {
                title: Some("No Nav Property".to_string()),
                ..Metadata::default()
            },
            manifest: Vec::new(),
            spine: Vec::new(),
            // Neither section carries the EPUB3 `nav` property in its
            // manifest entry: one is caught by the structural-entry
            // heuristic, the other by the ToC-shaped file name.
            sections: vec![
                make_section(
                    "toc-by-name",
                    0,
                    "Text/nav-2.xhtml",
                    toc_entries,
                    &mut blocks,
                ),
                make_section(
                    "toc-by-shape",
                    1,
                    "Text/indice-alternativo.xhtml",
                    (0..8).map(|n| format!("Chapter {n} · part {n}")).collect(),
                    &mut blocks,
                ),
                make_section(
                    "real-chapter",
                    2,
                    "Text/chapter-1.xhtml",
                    prose,
                    &mut blocks,
                ),
            ],
            blocks,
        };

        let chapters = chapters_from_book(&book);
        assert_eq!(chapters.len(), 1);
        assert!(chapters[0].text().starts_with("Il capitolo numero"));
    }

    #[test]
    fn structural_sections_are_not_narratable() {
        let nav = std::collections::HashSet::from(["text/navigation.xhtml".to_string()]);
        assert!(!is_narratable_section("content.opf", &nav));
        assert!(!is_narratable_section("OEBPS/package.OPF", &nav));
        assert!(!is_narratable_section("toc.ncx", &nav));
        assert!(!is_narratable_section("TEXT/navigation.xhtml#toc", &nav));
        assert!(!is_narratable_section("OEBPS/Text/navigation.xhtml", &nav));
        assert!(is_narratable_section("text/chapter-1.xhtml", &nav));
        assert!(is_narratable_section("chapter.html#frag", &nav));
        assert!(is_printed_toc_heading("INDICE"));
        assert!(is_printed_toc_entry("Cap. I"));
        assert!(is_roman_folio("XIV"));
        assert!(!is_roman_folio("INTRO"));
        assert!(is_printed_toc_entry(
            "Cap. I / Il Buonsenso Pag. 1 Cap. II / La natura Pag. 8"
        ));
        assert!(is_front_matter_heading("Nota dell’autore"));
        assert_eq!(
            chapter_label_key("lipu nanpa VI"),
            Some("lipu nanpa VI".to_string())
        );
        assert!(chapter_label_key("lipu nanpa 17 li toki e ni").is_none());
        assert_eq!(
            chapter_label_key("KAPITELO LUKA WAN"),
            Some("KAPITELO LUKA WAN".to_string())
        );
    }

    #[test]
    fn pdf_page_spine_is_grouped_by_unique_chapter_labels_and_drops_furniture() {
        use bookforge_core::ir::{
            Block, BlockId, BookFormat, BookId, DomPath, Metadata, Section, SectionId, TextRun,
        };

        let mut sections = Vec::new();
        let mut blocks = Vec::new();
        for page in 0..8 {
            let section_id = SectionId(format!("s{page}"));
            let mut block_ids = Vec::new();
            let page_texts = [
                (BlockKind::Heading(1), (page + 1).to_string()),
                (BlockKind::Paragraph, "Repeated Book Title".to_string()),
                (
                    BlockKind::Paragraph,
                    match page {
                        0 => "Preface".to_string(),
                        1 => "Repeated Book Title CAPITOLO I:".to_string(),
                        5 => "CAPITOLO II".to_string(),
                        _ => format!("page {page} prose"),
                    },
                ),
            ];
            for (part, (kind, text)) in page_texts.into_iter().enumerate() {
                let id = BlockId(format!("b{page}_{part}"));
                block_ids.push(id.clone());
                blocks.push(Block {
                    id,
                    section_id: section_id.clone(),
                    kind,
                    dom_path: DomPath(vec![part]),
                    text_runs: vec![TextRun {
                        id: format!("r{page}_{part}"),
                        text,
                    }],
                    inline_marks: Vec::new(),
                    protected_spans: Vec::new(),
                    token_estimate: 1,
                });
            }
            sections.push(Section {
                id: section_id,
                href: format!("page-{page}.xhtml"),
                spine_index: page,
                title: Some((page + 1).to_string()),
                heading_level: Some(1),
                block_ids,
                prev: None,
                next: None,
            });
        }
        let book = Book {
            source_path: None,
            id: BookId("pdf-pages".to_string()),
            format: BookFormat::Epub,
            metadata: Metadata {
                title: Some("Test Book".to_string()),
                creators: Vec::new(),
                language: Some("it".to_string()),
            },
            manifest: Vec::new(),
            spine: Vec::new(),
            sections,
            blocks,
        };

        let chapters = chapters_from_book(&book);
        assert_eq!(chapters.len(), 3);
        assert_eq!(chapters[0].title, "Test Book");
        assert_eq!(chapters[1].title, "CAPITOLO I:");
        assert_eq!(chapters[2].title, "CAPITOLO II");
        assert_eq!(chapters[1].blocks[0].kind, NarrationBlockKind::Title);
        assert_eq!(chapters[1].blocks[0].text, "CAPITOLO I:");
        assert_eq!(chapters[2].blocks[0].kind, NarrationBlockKind::Title);
        assert!(
            chapters
                .iter()
                .all(|chapter| !chapter.text().contains("Repeated Book Title"))
        );
        assert!(
            chapters
                .iter()
                .all(|chapter| chapter.text().lines().all(|line| {
                    let line = line.trim();
                    line.is_empty() || !line.chars().all(|ch| ch.is_ascii_digit())
                }))
        );
    }

    #[test]
    fn headings_are_typed_and_section_title_appears_once() {
        use bookforge_core::ir::{
            Block, BlockId, BookFormat, BookId, DomPath, Metadata, Section, SectionId, TextRun,
        };

        let section_id = SectionId("section".to_string());
        let make_block = |id: &str, kind, text: &str| Block {
            id: BlockId(id.to_string()),
            section_id: section_id.clone(),
            kind,
            dom_path: DomPath(vec![0]),
            text_runs: vec![TextRun {
                id: format!("{id}-run"),
                text: text.to_string(),
            }],
            inline_marks: Vec::new(),
            protected_spans: Vec::new(),
            token_estimate: 1,
        };
        let book = Book {
            source_path: None,
            id: BookId("headings".to_string()),
            format: BookFormat::Epub,
            metadata: Metadata::default(),
            manifest: Vec::new(),
            spine: Vec::new(),
            sections: vec![Section {
                id: section_id.clone(),
                href: "chapter.xhtml".to_string(),
                spine_index: 0,
                title: Some("The Title".to_string()),
                heading_level: Some(1),
                block_ids: vec![
                    BlockId("title".to_string()),
                    BlockId("body".to_string()),
                    BlockId("heading".to_string()),
                ],
                prev: None,
                next: None,
            }],
            blocks: vec![
                make_block("title", BlockKind::Heading(1), "The Title"),
                make_block("body", BlockKind::Paragraph, "Body text."),
                make_block("heading", BlockKind::Heading(2), "A Subheading"),
            ],
        };

        let chapters = chapters_from_book(&book);
        assert_eq!(chapters[0].blocks[0].kind, NarrationBlockKind::Title);
        assert_eq!(chapters[0].blocks[2].kind, NarrationBlockKind::Heading(2));
        assert_eq!(
            chapters[0]
                .blocks
                .iter()
                .filter(|block| block.text == "The Title")
                .count(),
            1
        );
    }

    #[test]
    fn chunk_blocks_keeps_titles_and_headings_separate_from_body() {
        let blocks = vec![
            NarrationBlock {
                kind: NarrationBlockKind::Title,
                text: "The Title".to_string(),
            },
            NarrationBlock {
                kind: NarrationBlockKind::Paragraph,
                text: "First body sentence.".to_string(),
            },
            NarrationBlock {
                kind: NarrationBlockKind::Heading(2),
                text: "A Heading".to_string(),
            },
            NarrationBlock {
                kind: NarrationBlockKind::Paragraph,
                text: "Second body sentence.".to_string(),
            },
        ];

        let chunks = chunk_blocks(&blocks, 100);
        assert_eq!(chunks[0].kind, ChunkKind::Title);
        assert_eq!(chunks[0].text, "The Title");
        assert_eq!(chunks[1].kind, ChunkKind::Body);
        assert_eq!(chunks[2].kind, ChunkKind::Heading);
        assert_eq!(chunks[2].text, "A Heading");
        assert_eq!(chunks[3].kind, ChunkKind::Body);
    }
}
