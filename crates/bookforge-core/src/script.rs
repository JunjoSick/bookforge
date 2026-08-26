//! Dominant-script detection, derived from the text rather than from a
//! declared language.
//!
//! Several parts of the pipeline need to know whether prose is written in a
//! cased script (Latin, Cyrillic, Greek) or a caseless one (Han, Kana, Hangul,
//! Thai, Arabic, Hebrew, Devanagari), because the two behave differently in
//! ways that matter: caseless scripts carry far more information per character
//! and cannot signal a proper noun through capitalization.
//!
//! Word spacing is a *separate* axis, and `is_space_delimited` measures it
//! separately. Arabic and Hebrew are caseless yet space-delimited; Han, Kana
//! and Hangul are neither. Conflating the two would change behaviour for
//! Arabic and Hebrew on no evidence, so nothing here infers one from the other.
//!
//! Deriving this from the characters, not from `--source`, is deliberate. A
//! declared language can be wrong, absent, or a label the project has never
//! enumerated, and the resulting behaviour should still be correct. Counting
//! the *dominant* kind rather than testing for the presence of any cased
//! character keeps an occasional Latin word — a name, a citation, a unit — from
//! rerouting an otherwise caseless book.
//!
//! Callers decide what a tie means, because the safe default differs by site:
//! glossary extraction treats an undetermined script as caseless to preserve
//! recall, while token estimation treats it as cased so it does not
//! underestimate. Returning the three-way class rather than a boolean is what
//! lets each keep the answer it needs.

/// How a body of text scores on the cased/caseless axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptClass {
    /// Predominantly a script that distinguishes upper and lower case.
    Cased,
    /// Predominantly a script that does not.
    Caseless,
    /// Equal counts, or no alphabetic characters to judge by.
    Undetermined,
}

impl ScriptClass {
    pub fn label(self) -> &'static str {
        match self {
            Self::Cased => "cased",
            Self::Caseless => "caseless",
            Self::Undetermined => "undetermined",
        }
    }
}

/// Fewer than this many spaces per alphabetic character means the text does not
/// delimit its words with whitespace. Latin prose sits near 0.18 (roughly one
/// space every five or six letters) and Arabic and Hebrew are similar; Han,
/// Kana and Hangul prose sits at or very near zero. The gap between those two
/// populations is an order of magnitude, so the exact cut is not delicate.
const UNSPACED_SPACE_RATIO: f64 = 0.02;

/// Whether the text delimits its words with whitespace.
///
/// This is measured, not inferred from the script class, because the two do not
/// coincide: Arabic, Hebrew, Thai and Devanagari are all caseless, but Arabic
/// and Hebrew are space-delimited while Han, Kana and Hangul are not. Treating
/// "caseless" as "unspaced" would quietly change behaviour for Arabic and
/// Hebrew sources on no evidence at all.
///
/// It matters because splitting on whitespace — or on runs of alphanumerics,
/// which for unspaced scripts amounts to splitting on punctuation — returns
/// whole clauses rather than words, and any threshold expressed in "words" then
/// means something entirely different.
pub fn is_space_delimited(text: &str) -> bool {
    let (alphabetic, spaces) = text
        .chars()
        .fold((0usize, 0usize), |(letters, spaces), ch| {
            if ch.is_alphabetic() {
                (letters + 1, spaces)
            } else if ch.is_whitespace() {
                (letters, spaces + 1)
            } else {
                (letters, spaces)
            }
        });
    // No letters means nothing to judge; treat it as ordinary spaced text so
    // callers keep their existing behaviour on numerals and punctuation.
    if alphabetic == 0 {
        return true;
    }
    spaces as f64 / alphabetic as f64 >= UNSPACED_SPACE_RATIO
}

/// Count alphabetic characters by whether their script has case.
///
/// Non-alphabetic characters — digits, punctuation, whitespace, symbols — are
/// ignored, so a table of numbers is `Undetermined` rather than miscounted.
pub fn script_counts(text: &str) -> (usize, usize) {
    text.chars()
        .filter(|ch| ch.is_alphabetic())
        .fold((0, 0), |(cased, caseless), ch| {
            if ch.is_lowercase() || ch.is_uppercase() {
                (cased + 1, caseless)
            } else {
                (cased, caseless + 1)
            }
        })
}

/// Classify text by its dominant script.
pub fn script_class(text: &str) -> ScriptClass {
    classify(script_counts(text))
}

/// Classify counts already accumulated elsewhere, for callers that walk a
/// large structure once rather than materialising its text.
pub fn classify((cased, caseless): (usize, usize)) -> ScriptClass {
    match cased.cmp(&caseless) {
        std::cmp::Ordering::Greater => ScriptClass::Cased,
        std::cmp::Ordering::Less => ScriptClass::Caseless,
        std::cmp::Ordering::Equal => ScriptClass::Undetermined,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_by_dominant_script() {
        assert_eq!(script_class("The quick brown fox"), ScriptClass::Cased);
        assert_eq!(script_class("Преступление и наказание"), ScriptClass::Cased);
        assert_eq!(script_class("矛盾的普遍性和特殊性"), ScriptClass::Caseless);
        assert_eq!(script_class("これは日本語の文です"), ScriptClass::Caseless);
        assert_eq!(script_class("한국어 문장입니다"), ScriptClass::Caseless);
    }

    #[test]
    fn an_occasional_latin_word_does_not_reroute_a_caseless_book() {
        assert_eq!(
            script_class("矛盾论的英文标题是 On Contradiction，作者是毛泽东，写于一九三七年。"),
            ScriptClass::Caseless
        );
    }

    #[test]
    fn text_without_alphabetic_evidence_is_undetermined() {
        assert_eq!(script_class(""), ScriptClass::Undetermined);
        assert_eq!(script_class("1234 — !? 56.78"), ScriptClass::Undetermined);
        // Exactly balanced: two cased letters, two caseless.
        assert_eq!(script_class("Ab 中国"), ScriptClass::Undetermined);
    }

    #[test]
    fn spacing_is_measured_rather_than_inferred_from_case() {
        assert!(is_space_delimited(
            "The quick brown fox jumps over the lazy dog"
        ));
        assert!(is_space_delimited("Преступление и наказание, роман"));
        assert!(!is_space_delimited(
            "矛盾的普遍性和特殊性的关系是矛盾问题的精髓"
        ));
        assert!(!is_space_delimited(
            "これは日本語の文章であり分かち書きをしない"
        ));
    }

    #[test]
    fn caseless_but_spaced_scripts_are_not_treated_as_unspaced() {
        // The distinction this function exists for: both are caseless, but
        // only one is written without spaces between words.
        assert_eq!(
            script_class("العربية لغة جميلة ومفيدة جدا"),
            ScriptClass::Caseless
        );
        assert!(is_space_delimited("العربية لغة جميلة ومفيدة جدا"));

        assert_eq!(
            script_class("עברית היא שפה יפה מאוד"),
            ScriptClass::Caseless
        );
        assert!(is_space_delimited("עברית היא שפה יפה מאוד"));
    }

    #[test]
    fn text_without_letters_is_treated_as_spaced() {
        assert!(is_space_delimited(""));
        assert!(is_space_delimited("1234 5678 90"));
    }
}
