//! Line-level bidirectional repair for RTL text recovered by pdftohtml.
//!
//! PDF-7: poppler emits the *characters* of each RTL fragment in
//! logical order, but reconstruction concatenates fragments by their
//! visual position (left → right). On a right-to-left line the visually
//! leftmost fragment comes last in reading order, so dominant-RTL lines
//! emerge scrambled into LTR word order. [`reorder_line_spans`]
//! restores reading order for such lines.
//!
//! Input contract (matching observed poppler behaviour and encoded in
//! this crate's programmatic fixtures): *a visual sequence of logical
//! runs*. Repair therefore needs run reordering, not a shaping engine:
//!
//! 1. Split the assembled line into whitespace-delimited tokens.
//! 2. Classify each token by its strongest script evidence: RTL token,
//!    LTR token, or neutral token (digits/symbols/standalone marks).
//! 3. Walk the visual sequence back-to-front emitting tokens in
//!    reverse; consecutive non-RTL tokens (Latin words, numerals) form
//!    a cluster whose internal order survives — exactly how embedded
//!    European numbers and Latin technical terms must behave inside
//!    RTL prose.
//! 4. Mirror paired brackets on pure-punctuation tokens: their sides
//!    re-label once surrounding text flows the other way (UAX #9 L4).
//!
//! Lines whose strong characters lean LTR pass through untouched, so
//! English documents pay nothing and mixed Hebrew/Latin lines only
//! flip when RTL dominates.
//!
//! Dependency note: `bookforge-core`'s script tooling (`ScriptClass`,
//! `is_space_delimited`) classifies scripts along the cased/caseless
//! and spaced/unspaced axes, where Arabic/Hebrew sit with CJK as
//! caseless. It deliberately has no *directional* axis, so RTL script
//! membership is classified locally here rather than reused.

use crate::model::Span;

/// Mirrored punctuation pairs swapped while reordering (UAX #9 L4).
const MIRRORED: [(char, char); 5] = [('(', ')'), ('[', ']'), ('{', '}'), ('<', '>'), ('«', '»')];

/// Whether a character belongs to a right-to-left alphabet.
pub fn is_rtl_letter(ch: char) -> bool {
    matches!(ch as u32,
        0x0590..=0x05FF // Hebrew
        | 0x0600..=0x06FF // Arabic
        | 0x0700..=0x074F // Syriac
        | 0x0750..=0x077F // Arabic supplement
        | 0x0780..=0x07BF // Thaana
        | 0x07C0..=0x07CF // NKo
        | 0x08A0..=0x08FF // Arabic extended-A
        | 0xFB50..=0xFDFF // Arabic presentation forms A
        | 0xFE70..=0xFEFF // Arabic presentation forms B
    )
}

/// Count alphabetic characters by RTL-script membership versus every
/// other alphabet, so pages can vote RTL without re-walking text.
pub fn rtl_letter_counts(text: &str) -> (usize, usize) {
    let mut rtl = 0usize;
    let mut other = 0usize;
    for ch in text.chars() {
        if is_rtl_letter(ch) {
            rtl += 1;
        } else if ch.is_alphabetic() {
            other += 1;
        }
    }
    (rtl, other)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenKind {
    Rtl,
    /// Everything with alphabetic evidence but no RTL letters: Latin,
    /// Cyrillic, Greek, CJK. CJK is caseless rather than directional,
    /// but inside an RTL line it clusters with non-reversed runs.
    Ltr,
    Neutral,
}

fn token_kind(token: &str) -> TokenKind {
    let (rtl, other) = rtl_letter_counts(token);
    match rtl.cmp(&other) {
        std::cmp::Ordering::Greater => TokenKind::Rtl,
        std::cmp::Ordering::Less => TokenKind::Ltr,
        std::cmp::Ordering::Equal => TokenKind::Neutral,
    }
}

fn line_is_rtl_dominant(text: &str) -> bool {
    let (rtl, other) = rtl_letter_counts(text);
    rtl > other && rtl >= 2
}

/// Flip paired brackets' open/close orientation (UAX #9 L4), applied
/// only to pure-punctuation tokens. Tokens carrying letters or digits
/// (`(2024)`, `(U.S.)`) embed their own direction and survive
/// byte-for-byte.
fn mirror_brackets(token: &str) -> String {
    let mut has_mirrorable = false;
    for ch in token.chars() {
        if MIRRORED
            .iter()
            .any(|(open, close)| *open == ch || *close == ch)
        {
            has_mirrorable = true;
            continue;
        }
        if !ch.is_whitespace() {
            return token.to_string();
        }
    }
    if !has_mirrorable {
        return token.to_string();
    }
    token
        .chars()
        .map(|ch| {
            for (open, close) in MIRRORED {
                if ch == open {
                    return close;
                }
                if ch == close {
                    return open;
                }
            }
            ch
        })
        .collect()
}

/// Reordered [`Span`] list for one visually-assembled line, or `None`
/// when the line is not RTL-dominant ("keep as-is").
///
/// Styled runs rebuild at whitespace-token granularity so bold/italic
/// attribution follows its own words through the reorder instead of
/// being flattened away; adjacent rebuilt runs sharing style merge
/// again.
pub fn reorder_line_spans(spans: &[Span]) -> Option<Vec<Span>> {
    let joined: String = spans.iter().map(|span| span.text.as_str()).collect();
    if !line_is_rtl_dominant(&joined) {
        return None;
    }

    // Explode styled spans into (word, style-tagged) pieces. A styled
    // run splits at whitespace only, keeping emphasis glued to its own
    // letters across reorder boundaries.
    let mut pieces: Vec<(&str, &Span)> = Vec::new();
    for span in spans {
        for word in span.text.split(' ').filter(|word| !word.is_empty()) {
            pieces.push((word, span));
        }
    }

    let mut logical: Vec<usize> = Vec::with_capacity(pieces.len());
    let mut pending_ltr: Vec<usize> = Vec::new();
    for index in (0..pieces.len()).rev() {
        if token_kind(pieces[index].0) == TokenKind::Rtl {
            flush_ltr_indices(&mut pending_ltr, &mut logical);
            logical.push(index);
        } else {
            pending_ltr.push(index);
        }
    }
    flush_ltr_indices(&mut pending_ltr, &mut logical);

    let mut out: Vec<Span> = Vec::new();
    for position in logical {
        let (text, style) = pieces[position];
        let text = mirror_brackets(text);
        if let Some(last) = out.last_mut()
            && last.bold == style.bold
            && last.italic == style.italic
        {
            last.text.push(' ');
            last.text.push_str(&text);
            continue;
        }
        out.push(Span {
            text,
            bold: style.bold,
            italic: style.italic,
        });
    }
    (!out.is_empty()).then_some(out)
}

/// Non-RTL tokens collected back-to-front restore their visual order —
/// which is already their internal reading order — when flushed ahead
/// of the next RTL token.
fn flush_ltr_indices(pending: &mut Vec<usize>, logical: &mut Vec<usize>) {
    if pending.is_empty() {
        return;
    }
    pending.reverse();
    logical.append(pending);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(text: &str) -> Span {
        Span {
            text: text.to_string(),
            bold: false,
            italic: false,
        }
    }

    fn reordered(text: &str) -> Option<String> {
        reorder_line_spans(&[plain(text)]).map(|spans| {
            spans
                .iter()
                .map(|span| span.text.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        })
    }

    #[test]
    fn hebrew_visual_order_is_restored_wordwise() {
        // Logical: פתחה ישראל משא. Visual assembly (left → right) reads
        // the sentence backwards; repair restores it.
        assert_eq!(
            reordered("משא ישראל פתחה").as_deref(),
            Some("פתחה ישראל משא")
        );
    }

    #[test]
    fn arabic_line_with_european_number_keeps_the_number() {
        // Logical: أصدر المعهد في عام 2024 تقريره السنوي
        assert_eq!(
            reordered("السنوي تقريره 2024 عام في المعهد أصدر").as_deref(),
            Some("أصدر المعهد في عام 2024 تقريره السنوي")
        );
    }

    #[test]
    fn embedded_latin_cluster_keeps_its_internal_order() {
        // In RTL prose an LTR run renders as a left-to-right block, so
        // the visual scan sees its words already in logical order and
        // they must survive verbatim.
        // Logical: ראשון שני Tesseract OCR שלישי רביעי חמישי
        let visual = "חמישי רביעי שלישי Tesseract OCR שני ראשון";
        assert_eq!(
            reordered(visual).as_deref(),
            Some("ראשון שני Tesseract OCR שלישי רביעי חמישי")
        );
    }

    #[test]
    fn isolated_bracket_tokens_mirror_but_parenthesized_numbers_do_not() {
        // A bracket-only fragment flips orientation with the line flow;
        // a parenthesized number keeps byte-identical content.
        assert_eq!(
            reordered("שני ) אחד").as_deref(),
            Some("אחד ( שני"),
            "bracket-only token must mirror"
        );

        assert_eq!(
            reordered("עברית )12( מספר").as_deref(),
            Some("מספר )12( עברית"),
            "numeric tokens must stay byte-identical"
        );
    }

    #[test]
    fn presentation_forms_are_rtl_evidence() {
        // Shaped Arabic output (U+FE70..FEFF) still votes the line RTL.
        let shaped = "\u{FEA9}\u{FEDF}\u{FE8E} \u{FE95}\u{FEE4}\u{FEAE}";
        let (rtl, other) = rtl_letter_counts(shaped);
        assert!(rtl >= 4 && other == 0, "{rtl}/{other}");
        assert!(reordered(shaped).is_some());
    }

    #[test]
    fn ltr_dominant_lines_are_left_untouched() {
        assert_eq!(reordered("The quick brown fox"), None);
        assert_eq!(reordered("R2D2 and C3PO"), None);
        assert_eq!(reordered("2024: a report"), None);
    }

    #[test]
    fn styled_spans_follow_their_words_through_the_reorder() {
        let spans = vec![
            plain("סוף "),
            Span {
                text: "מודגש".to_string(),
                bold: true,
                italic: false,
            },
            plain(" התחלה"),
        ];

        let reordered_spans = reorder_line_spans(&spans).expect("RTL spans");

        assert_eq!(reordered_spans.len(), 3);
        // Logical order begins with התחלה again, still unbolded.
        assert_eq!(reordered_spans[0].text, "התחלה");
        assert!(!reordered_spans[0].bold);
        assert_eq!(reordered_spans[1].text, "מודגש");
        assert!(reordered_spans[1].bold);
        assert_eq!(reordered_spans[2].text, "סוף");
        assert!(!reordered_spans[2].bold);
    }

    #[test]
    fn ltr_span_lists_return_none() {
        assert_eq!(reorder_line_spans(&[plain("Plain English text")]), None);
    }
}
