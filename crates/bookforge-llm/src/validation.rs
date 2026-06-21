use std::collections::HashMap;

use bookforge_core::marker::strip_marker_tokens;

const MIN_SOURCE_CHARS: usize = 120;
const MIN_OVERLAP_WORDS: usize = 30;
const COPIED_WORD_RATIO: f64 = 0.92;

pub(crate) fn should_validate_source_copy(
    provider: &str,
    source_language: Option<&str>,
    target_language: &str,
) -> bool {
    if provider.eq_ignore_ascii_case("mock") {
        return false;
    }
    source_language
        .map(|source| !source.eq_ignore_ascii_case(target_language))
        .unwrap_or(true)
}

pub(crate) fn source_copy_validation_error(
    source: &str,
    translation: &str,
    section_title: Option<&str>,
) -> Option<String> {
    if is_reference_section(section_title) {
        return None;
    }

    let source_normalized = normalized_prose(source);
    if source_normalized.chars().count() < MIN_SOURCE_CHARS {
        return None;
    }
    if looks_like_page_reference(&source_normalized)
        || looks_like_bilingual_gloss(&source_normalized)
    {
        return None;
    }
    let translation_normalized = normalized_prose(translation);
    if source_normalized == translation_normalized {
        return Some("translation is unchanged from the source-language prose".to_string());
    }

    let source_words = words(&source_normalized);
    if source_words.len() < MIN_OVERLAP_WORDS {
        return None;
    }
    let translation_words = words(&translation_normalized);
    let overlap = multiset_overlap(&source_words, &translation_words);
    let ratio = overlap as f64 / source_words.len() as f64;
    if overlap >= MIN_OVERLAP_WORDS && ratio >= COPIED_WORD_RATIO {
        return Some(format!(
            "translation retains {:.0}% of the source-language words",
            ratio * 100.0
        ));
    }

    None
}

fn normalized_prose(text: &str) -> String {
    strip_marker_tokens(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn words(text: &str) -> Vec<String> {
    text.split(|character: char| !character.is_alphanumeric() && character != '\'')
        .filter(|word| !word.is_empty())
        .map(|word| word.to_lowercase())
        .collect()
}

fn multiset_overlap(source: &[String], translation: &[String]) -> usize {
    let mut available = HashMap::<&str, usize>::new();
    for word in translation {
        *available.entry(word.as_str()).or_default() += 1;
    }
    source
        .iter()
        .filter(|word| {
            let Some(count) = available.get_mut(word.as_str()) else {
                return false;
            };
            if *count == 0 {
                return false;
            }
            *count -= 1;
            true
        })
        .count()
}

fn is_reference_section(section_title: Option<&str>) -> bool {
    let Some(title) = section_title else {
        return false;
    };
    let title = title.trim().to_lowercase();
    [
        "note",
        "notes",
        "endnote",
        "endnotes",
        "bibliography",
        "references",
        "works cited",
        "index",
    ]
    .iter()
    .any(|reference| {
        title == *reference
            || title.starts_with(&format!("{reference} "))
            || title.ends_with(&format!(" {reference}"))
    })
}

fn looks_like_page_reference(text: &str) -> bool {
    let text = text.trim_start().to_lowercase();
    let Some(rest) = text.strip_prefix("p.") else {
        return false;
    };
    let rest = rest.trim_start();
    let digit_count = rest.chars().take_while(char::is_ascii_digit).count();
    digit_count > 0 && rest[digit_count..].starts_with('.')
}

fn looks_like_bilingual_gloss(text: &str) -> bool {
    let text = text.to_lowercase();
    text.contains("or in english:") || text.contains("in english:")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = "This deliberately long English paragraph contains enough ordinary \
        prose to exercise untranslated-copy detection in the production response validator. It \
        repeats no special protected data and should be rejected when a provider returns it \
        unchanged instead of translating the body into the requested target language.";

    #[test]
    fn rejects_exact_long_source_copy() {
        let error = source_copy_validation_error(SOURCE, SOURCE, Some("Chapter 1"));
        assert_eq!(
            error.as_deref(),
            Some("translation is unchanged from the source-language prose")
        );
    }

    #[test]
    fn rejects_nearly_complete_source_copy() {
        let translation = SOURCE.replace("ordinary prose", "common prose");
        let error = source_copy_validation_error(SOURCE, &translation, Some("Chapter 1"));
        assert!(
            error
                .as_deref()
                .is_some_and(|message| message.contains("source-language words"))
        );
    }

    #[test]
    fn allows_real_translation_and_short_text() {
        assert!(
            source_copy_validation_error(
                SOURCE,
                "Questo paragrafo è stato tradotto correttamente in italiano.",
                Some("Chapter 1")
            )
            .is_none()
        );
        assert!(source_copy_validation_error("A short title", "A short title", None).is_none());
    }

    #[test]
    fn allows_reference_sections() {
        assert!(source_copy_validation_error(SOURCE, SOURCE, Some("Endnotes")).is_none());
        assert!(source_copy_validation_error(SOURCE, SOURCE, Some("Bibliography")).is_none());
    }

    #[test]
    fn allows_page_references_without_section_metadata() {
        let reference = "p. 28. Brick by brick, the fireplaces. For an overview of how this works, \
            see Example Author, 'A Long Article Title That Should Remain in Its Published Language', \
            Journal of Example Studies 41 (2016): 425-452.";
        assert!(source_copy_validation_error(reference, reference, None).is_none());
    }

    #[test]
    fn allows_explicit_bilingual_glosses() {
        let gloss = "“todo lugar é são paulo—em todo lugar resistência—resista brasil—a turquia \
            está ao seu lado!” Or in English: “the whole world is São Paulo—resistance everywhere—\
            Brazil, resist!—Turkey is by your side.”";
        assert!(source_copy_validation_error(gloss, gloss, Some("Chapter 8")).is_none());
    }

    #[test]
    fn enables_only_real_cross_language_runs() {
        assert!(should_validate_source_copy(
            "deepseek",
            Some("English"),
            "Italian"
        ));
        assert!(!should_validate_source_copy(
            "deepseek",
            Some("Italian"),
            "italian"
        ));
        assert!(!should_validate_source_copy(
            "mock",
            Some("English"),
            "Italian"
        ));
    }
}
