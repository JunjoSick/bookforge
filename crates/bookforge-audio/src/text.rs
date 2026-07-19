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

/// One narratable chapter: the visible prose of a single spine section,
/// with a display title used for filenames and (when stitched) chapter
/// markers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chapter {
    /// Zero-based position in reading order.
    pub index: usize,
    pub title: String,
    /// Clean prose with inline markers removed and blocks joined by blank
    /// lines. Empty for sections that carry no readable text (cover images,
    /// nav documents).
    pub text: String,
}

impl Chapter {
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
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
            let mut paragraphs: Vec<String> = Vec::new();
            for block_id in &section.block_ids {
                let Some(block) = block_index.get(block_id) else {
                    continue;
                };
                if block.kind == BlockKind::PageFurniture {
                    continue;
                }
                let text = clean_block_text(block);
                if !text.is_empty() {
                    paragraphs.push(text);
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
                text: paragraphs.join("\n\n"),
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
    let mut paragraphs = Vec::<String>::new();
    let mut printed_toc = false;
    let mut printed_toc_roman_folio = false;

    for section in sections {
        let mut page_start = paragraphs.len();
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
                if !paragraphs.is_empty() {
                    chapters.push(Chapter {
                        index: chapters.len(),
                        title,
                        text: paragraphs.join("\n\n"),
                    });
                    paragraphs.clear();
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
            paragraphs.push(text);
        }

        // Join only a paragraph that actually crosses a physical page. Do
        // not flatten ordinary paragraph boundaries within the page.
        if page_start > 0
            && paragraphs.len() > page_start
            && should_join_across_page(&paragraphs[page_start - 1], &paragraphs[page_start])
        {
            let right = paragraphs.remove(page_start);
            paragraphs[page_start - 1].push(' ');
            paragraphs[page_start - 1].push_str(&right);
        }
    }

    if !paragraphs.is_empty() {
        chapters.push(Chapter {
            index: chapters.len(),
            title,
            text: paragraphs.join("\n\n"),
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
pub fn chunk_text(text: &str, max_chars: usize) -> Vec<String> {
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
/// `?`, and `…`; a boundary is only taken when the terminator is followed
/// by whitespace or end of input, which keeps decimals and abbreviations
/// like "3.14" or "e.g." intact. Blank lines also force a boundary so
/// paragraph structure is respected.
fn split_sentences(text: &str) -> Vec<String> {
    let mut sentences: Vec<String> = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = text.chars().collect();

    for (i, &ch) in chars.iter().enumerate() {
        current.push(ch);
        let is_terminator = matches!(ch, '.' | '!' | '?' | '…');
        if is_terminator {
            let next_is_break = chars
                .get(i + 1)
                .map(|next| next.is_whitespace())
                .unwrap_or(true);
            if next_is_break {
                push_trimmed(&mut sentences, &mut current);
            }
        } else if ch == '\n' {
            // A paragraph break (blank line) is a hard boundary even without
            // terminal punctuation, e.g. headings.
            if chars.get(i + 1).is_none_or(|next| *next == '\n') {
                push_trimmed(&mut sentences, &mut current);
            }
        }
    }
    push_trimmed(&mut sentences, &mut current);
    sentences
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
fn split_long_unit(unit: &str, max_chars: usize) -> Vec<String> {
    let mut pieces: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize;

    for word in unit.split_whitespace() {
        let word_len = word.chars().count();
        if word_len > max_chars {
            if current_len > 0 {
                pieces.push(std::mem::take(&mut current));
                current_len = 0;
            }
            pieces.extend(split_by_chars(word, max_chars));
            continue;
        }
        let separator = usize::from(current_len > 0);
        if current_len + separator + word_len > max_chars {
            pieces.push(std::mem::take(&mut current));
            current_len = 0;
        }
        if current_len > 0 {
            current.push(' ');
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
        assert!(
            chapters
                .iter()
                .all(|chapter| !chapter.text.contains("Repeated Book Title"))
        );
        assert!(
            chapters
                .iter()
                .all(|chapter| chapter.text.lines().all(|line| {
                    let line = line.trim();
                    line.is_empty() || !line.chars().all(|ch| ch.is_ascii_digit())
                }))
        );
    }
}
