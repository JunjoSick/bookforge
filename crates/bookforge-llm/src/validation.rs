use std::collections::HashMap;

use bookforge_core::marker::{marker_inner_texts, marker_reference_token, strip_marker_tokens};

const MIN_SOURCE_CHARS: usize = 120;
const MIN_OVERLAP_WORDS: usize = 30;
const COPIED_WORD_RATIO: f64 = 0.92;

/// Coarse family of an invalid model response, used to pick a user-facing
/// sentence instead of leaking serde internals like "missing field
/// `translation` at line 1 column 157" into persisted segment rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InvalidResponseCategory {
    /// A required field for one or more blocks was absent from the payload.
    MissingField,
    /// The payload was not the JSON structure the batch contract requires.
    InvalidJson,
    /// The payload parsed but violated the document's structural contract
    /// (markers, protected spans, the requested item set, run structure).
    MarkerMismatch,
    /// Anything else, including truncated or empty responses.
    Unknown,
}

impl InvalidResponseCategory {
    fn friendly_sentence(self) -> &'static str {
        match self {
            Self::MissingField => {
                "The model's response was missing a required field for one or more blocks \
                 — the batch will be retried or repaired."
            }
            Self::InvalidJson => {
                "The model returned malformed JSON instead of the expected batch format \
                 — the batch will be retried, split, or repaired."
            }
            Self::MarkerMismatch => {
                "The response did not preserve the document's required structure (inline \
                 markers, protected spans, or the requested set of items) — the batch will \
                 be split or the affected items repaired."
            }
            Self::Unknown => {
                "The model returned a response that could not be used as-is \
                 — the batch will be retried or repaired."
            }
        }
    }
}

/// Classify a raw invalid-response message. Raw serde and parse phrases are
/// matched by their stable substrings; unknown text degrades to `Unknown`
/// rather than being mislabeled.
pub(crate) fn invalid_response_category(raw: &str) -> InvalidResponseCategory {
    let lowered = raw.to_ascii_lowercase();

    // Specific structural failures outrank generic parse noise: a message
    // like "invalid batch JSON: missing field `translation` at line 1 column
    // 157" is a missing-field failure even though it also mentions JSON.
    const STRUCTURAL_PHRASES: [&str; 12] = [
        "batch response incomplete",
        "batch translation block mismatch",
        "duplicate item id in batch response",
        "run count mismatch",
        "unknown run id",
        "missing run id",
        "changed marker run",
        "inline marker",
        "protected span",
        "marker structure",
        "lost its reference text",
        "unknown item id",
    ];
    for phrase in STRUCTURAL_PHRASES {
        if lowered.contains(phrase) {
            return InvalidResponseCategory::MarkerMismatch;
        }
    }

    const MISSING_FIELD_PHRASES: [&str; 3] =
        ["missing field", "invalid length 0", "duplicate field"];
    for phrase in MISSING_FIELD_PHRASES {
        if lowered.contains(phrase) {
            return InvalidResponseCategory::MissingField;
        }
    }

    const INVALID_JSON_PHRASES: [&str; 11] = [
        "invalid batch json",
        "json parse failed",
        "eof while parsing",
        "trailing characters",
        "expected value",
        "expected ident",
        "expected `:`",
        "expected `,` or `}`",
        "expected `,` or `]`",
        "key must be a string",
        "control character",
    ];
    for phrase in INVALID_JSON_PHRASES {
        if lowered.contains(phrase) {
            return InvalidResponseCategory::InvalidJson;
        }
    }

    InvalidResponseCategory::Unknown
}

/// Map a raw invalid-response message to a user-facing sentence. Raw detail
/// is never included in the result — callers keep it available through the
/// error chain and `tracing::debug!`.
pub(crate) fn friendly_validation_error(raw: &str) -> String {
    // Truncation and empty-content failures have their own precise remedies;
    // give them specific sentences even though they have no serde signature.
    // The empty-content check runs first: its message names the output
    // budget, which would otherwise be mistaken for truncation.
    let lowered = raw.to_ascii_lowercase();
    if lowered.starts_with("the model produced no content") {
        return "The model produced no usable content for this batch \
                — the batch will be retried; if this repeats, raise the output-token \
                budget or lower the batch size."
            .to_string();
    }
    if lowered.contains("truncated") || lowered.contains("output budget") {
        return "The model's response was cut off before it finished — the output-token \
                budget was exhausted; the batch will be retried with a larger budget or \
                split into smaller pieces."
            .to_string();
    }
    invalid_response_category(raw)
        .friendly_sentence()
        .to_string()
}

const TOKI_PONA_WORDS: &[&str] = &[
    "a",
    "akesi",
    "ala",
    "alasa",
    "ale",
    "ali",
    "anpa",
    "ante",
    "anu",
    "awen",
    "e",
    "en",
    "esun",
    "ijo",
    "ike",
    "ilo",
    "insa",
    "jaki",
    "jan",
    "jelo",
    "jo",
    "kala",
    "kalama",
    "kama",
    "kasi",
    "ken",
    "kepeken",
    "kili",
    "kiwen",
    "ko",
    "kon",
    "kule",
    "kulupu",
    "kute",
    "la",
    "lape",
    "laso",
    "lawa",
    "len",
    "lete",
    "li",
    "lili",
    "linja",
    "lipu",
    "loje",
    "lon",
    "luka",
    "lukin",
    "lupa",
    "ma",
    "mama",
    "mani",
    "meli",
    "mi",
    "mije",
    "moku",
    "moli",
    "monsi",
    "mu",
    "mun",
    "musi",
    "mute",
    "nanpa",
    "nasa",
    "nasin",
    "nena",
    "ni",
    "nimi",
    "noka",
    "o",
    "olin",
    "ona",
    "open",
    "pakala",
    "pali",
    "palisa",
    "pan",
    "pana",
    "pi",
    "pilin",
    "pimeja",
    "pini",
    "pipi",
    "poka",
    "poki",
    "pona",
    "pu",
    "sama",
    "seli",
    "selo",
    "seme",
    "sewi",
    "sijelo",
    "sike",
    "sin",
    "sina",
    "sinpin",
    "sitelen",
    "sona",
    "soweli",
    "suli",
    "suno",
    "supa",
    "suwi",
    "tan",
    "taso",
    "tawa",
    "telo",
    "tenpo",
    "toki",
    "tomo",
    "tu",
    "unpa",
    "uta",
    "utala",
    "walo",
    "wan",
    "waso",
    "wawa",
    "weka",
    "wile",
    // Commonly documented nimi ku suli / established extended words. The
    // built-in style permits established vocabulary, but never ad-hoc
    // Italian-looking technical coinages.
    "epiku",
    "jasima",
    "kijetesantakalu",
    "kin",
    "kipisi",
    "kokosila",
    "ku",
    "lanpan",
    "leko",
    "linluwi",
    "meso",
    "misikeke",
    "monsuta",
    "n",
    "namako",
    "oko",
    "pake",
    "soko",
    "tonsi",
];

const ITALIAN_STOP_WORDS: &[&str] = &[
    "ad", "agli", "ai", "al", "alla", "alle", "allo", "anche", "avere", "che", "chi", "ci", "come",
    "con", "contro", "cui", "da", "dal", "dalla", "dalle", "dagli", "dei", "del", "della", "delle",
    "degli", "di", "dove", "dopo", "ed", "era", "erano", "essere", "fa", "fare", "fatto", "fino",
    "fra", "gli", "ha", "hanno", "ho", "il", "in", "io", "la", "le", "lei", "lo", "loro", "lui",
    "ma", "molto", "nei", "nel", "nella", "nelle", "negli", "noi", "non", "ogni", "per", "perche",
    "poi", "prima", "puo", "quale", "quando", "quanto", "quello", "quella", "quelli", "quelle",
    "questa", "queste", "questi", "questo", "qui", "se", "sei", "senza", "sia", "siano", "siamo",
    "siete", "sono", "sotto", "su", "sul", "sulla", "sulle", "tra", "tre", "tu", "tutti", "tutto",
    "una", "uno", "voi",
];

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
    // Identifiers, numeric headings, and ornamental separators have no
    // source-language prose to translate. Their preservation is checked by
    // the structural/protected-span validators, not the language-copy gate.
    if !source_normalized.chars().any(char::is_alphabetic)
        || looks_like_page_reference(&source_normalized)
        || looks_like_bilingual_gloss(&source_normalized)
    {
        return None;
    }
    let translation_normalized = normalized_prose(translation);
    if !source_normalized.is_empty()
        && source_normalized.eq_ignore_ascii_case(&translation_normalized)
    {
        return Some("translation is unchanged from the source-language prose".to_string());
    }
    if source_normalized.chars().count() < MIN_SOURCE_CHARS {
        return None;
    }

    let (source_units, translation_units, unit) =
        comparison_units(&source_normalized, &translation_normalized);
    if source_units.len() < MIN_OVERLAP_WORDS {
        return None;
    }
    let overlap = multiset_overlap(&source_units, &translation_units);
    let ratio = overlap as f64 / source_units.len() as f64;
    if overlap >= MIN_OVERLAP_WORDS && ratio >= COPIED_WORD_RATIO {
        return Some(format!(
            "translation retains {:.0}% of the source-language {unit}",
            ratio * 100.0
        ));
    }

    None
}

/// Choose the unit the near-copy comparison counts in, based on whether the
/// source delimits its words with whitespace.
///
/// `words` splits on runs of alphanumerics, which is only a word count for text
/// that puts spaces between words. Han, Kana and Hangul characters are
/// alphanumeric and unspaced, so a whole clause comes back as one "word" and
/// the `MIN_OVERLAP_WORDS` floor is never cleared: measured across the two
/// Chinese books in the corpus, only 24% and 67% of substantial paragraphs
/// reached the overlap test at all, against 83-88% for English and Italian.
/// The detector was therefore silently switched off for exactly the material
/// it was most needed on, since a model that echoes an unspaced source stops
/// being caught by the exact-copy check the moment it alters one token.
///
/// For unspaced text the comparison runs on adjacent character pairs instead.
/// Bigrams are the standard unit for this: they track a near-copy closely, and
/// a genuine translation into a different script shares essentially none of
/// them. The test is on spacing rather than on case, because Arabic and Hebrew
/// are caseless yet space-delimited and should keep counting words.
fn comparison_units(source: &str, translation: &str) -> (Vec<String>, Vec<String>, &'static str) {
    if !bookforge_core::script::is_space_delimited(source) {
        (
            character_bigrams(source),
            character_bigrams(translation),
            "character pairs",
        )
    } else {
        (words(source), words(translation), "words")
    }
}

/// Adjacent pairs of the text's alphanumeric characters, in order. Punctuation
/// and whitespace are dropped first so that a re-punctuated copy still matches.
fn character_bigrams(text: &str) -> Vec<String> {
    let characters = text
        .chars()
        .filter(|ch| ch.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<Vec<_>>();
    characters
        .windows(2)
        .map(|pair| pair.iter().collect())
        .collect()
}

pub(crate) fn empty_translation_validation_error(
    source: &str,
    translation: &str,
) -> Option<&'static str> {
    (!source.trim().is_empty() && translation.trim().is_empty())
        .then_some("empty translation for non-empty source")
}

const KNOWN_QA_VERDICTS: [&str; 3] = ["pass", "warn", "fail"];

/// Validate a model-supplied QA verdict against the documented set
/// (`pass | warn | fail`). Anything else is unvalidated freeform today;
/// instead of letting it flow verbatim into reports and gates, degrade it
/// to `warn` (the conservative default that keeps human eyes on the
/// segment) and attach an explanatory finding.
pub(crate) fn normalize_qa_verdict(raw: &str) -> (&'static str, Option<String>) {
    let folded = raw.trim().to_ascii_lowercase();
    if let Some(known) = KNOWN_QA_VERDICTS.iter().find(|known| **known == folded) {
        return (known, None);
    }
    (
        "warn",
        Some(format!(
            "QA provider returned an unrecognized verdict {raw:?}; treated as warn"
        )),
    )
}

/// Conservative hard gates for the built-in Toki Pona style. These checks
/// deliberately cover failures that can be detected without judging the
/// translation's politics or semantic choices: source-language leakage,
/// invented lowercase vocabulary, invalid particle patterns, and runaway
/// repetition. Proper names remain allowed when capitalized.
pub(crate) fn target_language_validation_error(
    target_language: &str,
    source: &str,
    translation: &str,
    protected_spans: &[String],
) -> Option<String> {
    if !target_language.trim().eq_ignore_ascii_case("Toki Pona") {
        return None;
    }

    let mut prose = strip_marker_tokens(translation);
    for span in protected_spans {
        prose = redact_protected_span(&prose, span);
    }
    let raw_words = word_tokens(&prose);
    if raw_words.is_empty() {
        return None;
    }
    let lower = raw_words
        .iter()
        .map(|word| fold_latin_word(word))
        .collect::<Vec<_>>();
    let source_web_words = source_web_words(source);
    let source_words = word_tokens(source)
        .into_iter()
        .collect::<std::collections::HashSet<_>>();
    let italian = lower
        .iter()
        .filter(|word| {
            ITALIAN_STOP_WORDS.contains(&word.as_str()) && !TOKI_PONA_WORDS.contains(&word.as_str())
        })
        .count();
    if italian >= 3 || (raw_words.len() >= 12 && italian * 8 >= raw_words.len()) {
        return Some(format!(
            "Toki Pona translation retains {italian} Italian function words"
        ));
    }

    for (index, (raw, folded)) in raw_words.iter().zip(&lower).enumerate() {
        let proper_name = raw.chars().next().is_some_and(char::is_uppercase)
            && raw.chars().skip(1).any(char::is_lowercase)
            && index > 0
            && matches!(
                lower[index - 1].as_str(),
                "jan" | "ma" | "kulupu" | "lipu" | "nimi" | "toki" | "nasin" | "soweli"
            );
        let roman_numeral = is_roman_numeral(raw);
        let preserved_acronym = raw.len() <= 5
            && raw.chars().all(|ch| ch.is_ascii_uppercase())
            && !ITALIAN_STOP_WORDS.contains(&folded.as_str())
            && source_words.contains(raw);
        let preserved_web_word = source_web_words.iter().any(|word| word == folded);
        if !proper_name
            && !roman_numeral
            && !preserved_acronym
            && !preserved_web_word
            && !TOKI_PONA_WORDS.contains(&folded.as_str())
        {
            return Some(format!(
                "unapproved lowercase word in strict Toki Pona output: {raw}"
            ));
        }
    }

    for window in lower.windows(2) {
        if matches!(window, [subject, particle] if (subject == "mi" || subject == "sina") && particle == "li")
        {
            return Some(format!(
                "bare Toki Pona subject '{}' must not be followed by li",
                window[0]
            ));
        }
    }

    if let Some(word) = adjacent_repeated_word(&prose) {
        return Some(format!("pathological repeated Toki Pona word: {word}"));
    }

    if let Some(error) = short_pi_phrase_error(&prose) {
        return Some(error);
    }
    if let Some(error) = non_subject_en_error(&prose) {
        return Some(error);
    }
    if raw_words.len() == 1
        && matches!(
            lower[0].as_str(),
            "li" | "e" | "pi" | "la" | "en" | "o" | "anu"
        )
    {
        return Some(format!(
            "orphan Toki Pona grammatical particle: {}",
            lower[0]
        ));
    }

    None
}

fn adjacent_repeated_word(text: &str) -> Option<String> {
    let words = word_spans_outside_markers(text);
    words.windows(3).find_map(|window| {
        (window[0].lower == window[1].lower
            && window[1].lower == window[2].lower
            && text[window[0].end..window[1].start]
                .chars()
                .all(char::is_whitespace)
            && text[window[1].end..window[2].start]
                .chars()
                .all(char::is_whitespace))
        .then(|| window[0].lower.clone())
    })
}

#[derive(Debug)]
struct WordSpan {
    start: usize,
    end: usize,
    lower: String,
}

fn word_spans_outside_markers(text: &str) -> Vec<WordSpan> {
    let mut spans = Vec::new();
    let mut start = None;
    let mut in_marker = false;
    let url_ranges = protected_url_ranges(text);
    let mut url_index = 0usize;
    for (index, ch) in text.char_indices() {
        while url_ranges
            .get(url_index)
            .is_some_and(|(_, end)| *end <= index)
        {
            url_index += 1;
        }
        if url_ranges
            .get(url_index)
            .is_some_and(|(url_start, url_end)| *url_start <= index && index < *url_end)
        {
            if let Some(word_start) = start.take() {
                spans.push(WordSpan {
                    start: word_start,
                    end: index,
                    lower: fold_latin_word(&text[word_start..index]),
                });
            }
            continue;
        }
        if in_marker {
            if ch == '>' {
                in_marker = false;
            }
            continue;
        }
        if ch == '<' {
            if let Some(word_start) = start.take() {
                spans.push(WordSpan {
                    start: word_start,
                    end: index,
                    lower: fold_latin_word(&text[word_start..index]),
                });
            }
            in_marker = true;
        } else if ch.is_alphabetic() {
            start.get_or_insert(index);
        } else if let Some(word_start) = start.take() {
            spans.push(WordSpan {
                start: word_start,
                end: index,
                lower: fold_latin_word(&text[word_start..index]),
            });
        }
    }
    if let Some(word_start) = start {
        spans.push(WordSpan {
            start: word_start,
            end: text.len(),
            lower: fold_latin_word(&text[word_start..]),
        });
    }
    spans
}

fn protected_url_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut starts = text
        .match_indices("https://")
        .chain(text.match_indices("http://"))
        .map(|(start, _)| start)
        .collect::<Vec<_>>();
    starts.sort_unstable();
    starts
        .into_iter()
        .map(|start| {
            let end = text[start..]
                .char_indices()
                .find(|(_, ch)| ch.is_whitespace() || matches!(ch, '<' | '"' | '“' | '”'))
                .map_or(text.len(), |(offset, _)| start + offset);
            (start, end)
        })
        .collect()
}

fn source_web_words(source: &str) -> Vec<String> {
    source
        .split_whitespace()
        .map(|token| {
            token.trim_matches(|ch: char| {
                ch.is_ascii_punctuation() && !matches!(ch, '/' | ':' | '.' | '-' | '_')
            })
        })
        .filter(|token| {
            token.contains("://")
                || token.contains('/')
                || [".com", ".org", ".net", ".edu", ".gov", ".it", ".eu", ".uk"]
                    .iter()
                    .any(|suffix| token.to_ascii_lowercase().contains(suffix))
        })
        .flat_map(word_tokens)
        .map(|word| fold_latin_word(&word))
        .filter(|word| {
            word.len() >= 4
                || matches!(
                    word.as_str(),
                    "com" | "org" | "net" | "edu" | "gov" | "eu" | "it" | "uk"
                )
        })
        .collect()
}

fn is_roman_numeral(word: &str) -> bool {
    !word.is_empty()
        && word.len() <= 12
        && word
            .chars()
            .all(|ch| matches!(ch, 'I' | 'V' | 'X' | 'L' | 'C' | 'D' | 'M'))
}

fn redact_protected_span(text: &str, span: &str) -> String {
    if span.is_empty() {
        return text.to_string();
    }
    if span.chars().all(char::is_alphanumeric) {
        let mut output = String::with_capacity(text.len());
        let mut cursor = 0usize;
        for (start, _) in text.match_indices(span) {
            let end = start + span.len();
            let left_boundary = text[..start]
                .chars()
                .next_back()
                .is_none_or(|ch| !ch.is_alphanumeric());
            let right_boundary = text[end..]
                .chars()
                .next()
                .is_none_or(|ch| !ch.is_alphanumeric());
            if left_boundary && right_boundary {
                output.push_str(&text[cursor..start]);
                output.push(' ');
                cursor = end;
            }
        }
        output.push_str(&text[cursor..]);
        output
    } else {
        text.replace(span, " ")
    }
}

fn word_tokens(text: &str) -> Vec<String> {
    let mut output = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_alphabetic() || matches!(ch, '\'' | '’') {
            current.push(ch);
        } else if !current.is_empty() {
            output.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        output.push(current);
    }
    output
}

fn fold_latin_word(word: &str) -> String {
    word.to_lowercase()
        .chars()
        .map(|ch| match ch {
            'à' | 'á' | 'â' | 'ä' => 'a',
            'è' | 'é' | 'ê' | 'ë' => 'e',
            'ì' | 'í' | 'î' | 'ï' => 'i',
            'ò' | 'ó' | 'ô' | 'ö' => 'o',
            'ù' | 'ú' | 'û' | 'ü' => 'u',
            '’' => '\'',
            other => other,
        })
        .collect()
}

fn short_pi_phrase_error(text: &str) -> Option<String> {
    let tokens = toki_tokens(text);
    for (index, token) in tokens.iter().enumerate() {
        if token != "pi" {
            continue;
        }
        let following = tokens[index + 1..]
            .iter()
            .take_while(|word| {
                !matches!(
                    word.as_str(),
                    "." | "!"
                        | "?"
                        | ";"
                        | ":"
                        | "\""
                        | "“"
                        | "”"
                        | "("
                        | ")"
                        | "–"
                        | "—"
                        | "li"
                        | "e"
                        | "la"
                        | "en"
                        | "o"
                )
            })
            .filter(|word| word.chars().any(char::is_alphabetic))
            .count();
        if following < 2 {
            let context_start = index.saturating_sub(8);
            let context_end = (index + 10).min(tokens.len());
            return Some(format!(
                "pi must group at least two following words; offending context: {}",
                tokens[context_start..context_end].join(" ")
            ));
        }
    }
    None
}

fn non_subject_en_error(text: &str) -> Option<String> {
    for sentence in text.split(['.', '!', '?', ';', ':', '—', '–']) {
        let words = word_tokens(sentence)
            .into_iter()
            .map(|word| fold_latin_word(&word))
            .collect::<Vec<_>>();
        for (index, word) in words.iter().enumerate() {
            if word != "en" {
                continue;
            }
            let before = &words[..index];
            let after = &words[index + 1..];
            let local_start = before
                .iter()
                .rposition(|word| matches!(word.as_str(), "la" | "o"))
                .map_or(0, |position| position + 1);
            let local_before = &before[local_start..];
            let first_boundary = after
                .iter()
                .find(|word| matches!(word.as_str(), "li" | "e" | "la" | "o"));
            let subject_join = first_boundary.is_some_and(|word| word == "li")
                && !local_before
                    .iter()
                    .any(|word| matches!(word.as_str(), "li" | "e"));
            if !subject_join {
                return Some("en may only coordinate subjects in strict Toki Pona".to_string());
            }
        }
    }
    None
}

fn toki_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_alphabetic() {
            current.push(ch.to_ascii_lowercase());
        } else {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            if is_toki_phrase_boundary_char(ch) {
                tokens.push(ch.to_string());
            }
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn is_toki_phrase_boundary_char(ch: char) -> bool {
    matches!(
        ch,
        '.' | '!' | '?' | ';' | ':' | '"' | '“' | '”' | '(' | ')' | '–' | '—'
    )
}

/// Reject translations that kept a marker's ID but silently dropped the
/// reference text it wrapped. Endnote references arrive as
/// `word.<sup><a epub:type="noteref">*2</a></sup>`, i.e. `word.<m0><m1>*2</m1></m0>`,
/// so every marker-ID check can pass while the `*2` is gone from the finished
/// book. Marker prose stays free to change; only reference tokens are pinned.
pub(crate) fn marker_reference_text_error(source: &str, translation: &str) -> Option<String> {
    let translated_inner_texts = marker_inner_texts(translation)
        .into_iter()
        .map(|marker| (marker.id, marker.text))
        .collect::<HashMap<_, _>>();

    for marker in marker_inner_texts(source) {
        let Some(token) = marker_reference_token(&marker.text) else {
            continue;
        };
        let Some(translated_text) = translated_inner_texts.get(&marker.id) else {
            continue;
        };
        let compact_token = remove_whitespace(token);
        let compact_translation = remove_whitespace(translated_text);
        if exact_protected_span_present(&compact_token, &compact_translation)
            || numeric_tokens_equivalent(&compact_token, &compact_translation)
        {
            continue;
        }
        return Some(format!(
            "inline marker {} lost its reference text '{token}'",
            marker.id
        ));
    }

    None
}

fn remove_whitespace(value: &str) -> String {
    value.chars().filter(|ch| !ch.is_whitespace()).collect()
}

fn numeric_tokens_equivalent(left: &str, right: &str) -> bool {
    let left_forms = canonical_number_forms(left);
    let right_forms = canonical_number_forms(right);
    !left_forms.is_empty() && left_forms.iter().any(|form| right_forms.contains(form))
}

pub(crate) fn protected_span_present(span: &str, translation: &str) -> bool {
    dangling_numeric_span(span)
        || exact_protected_span_present(span, translation)
        || compact_numeric_punctuation_span(span)
            .is_some_and(|expected| compact_ascii_whitespace(translation).contains(&expected))
        || canonical_decimal_number(span).is_some_and(|expected| {
            numeric_runs(translation)
                .iter()
                .any(|candidate| canonical_decimal_number(candidate).as_deref() == Some(&expected))
        })
        || numeric_span_present(span, translation)
}

fn exact_protected_span_present(span: &str, translation: &str) -> bool {
    translation.match_indices(span).any(|(start, _)| {
        let end = start + span.len();
        let left_boundary = span
            .chars()
            .next()
            .is_none_or(|first| !first.is_alphanumeric())
            || translation[..start]
                .chars()
                .next_back()
                .is_none_or(|ch| !ch.is_alphanumeric());
        let right_boundary = span
            .chars()
            .next_back()
            .is_none_or(|last| !last.is_alphanumeric())
            || translation[end..]
                .chars()
                .next()
                .is_none_or(|ch| !ch.is_alphanumeric());
        left_boundary && right_boundary
    })
}

/// Every canonical reading of one numeric token. Empty when `token` is not a number.
///
/// Separators are classified rather than replaced blindly: repeated identical
/// separators can be strict three-digit grouping, while a final `.` or `,`
/// can be decimal after differently styled grouping. A single separator with
/// a three-digit suffix deliberately yields both readings.
fn canonical_number_forms(token: &str) -> Vec<String> {
    let normalized = normalize_number_signs(token);
    let (sign, unsigned) = if let Some(unsigned) = normalized.strip_prefix('-') {
        ("-", unsigned)
    } else if let Some(unsigned) = normalized.strip_prefix('+') {
        ("+", unsigned)
    } else {
        ("", normalized.as_str())
    };
    let (numeric, percent) = if let Some(numeric) = unsigned.strip_suffix('%') {
        (numeric, "%")
    } else {
        (unsigned, "")
    };
    if numeric.is_empty() {
        return Vec::new();
    }

    let mut groups = Vec::new();
    let mut separators = Vec::new();
    let mut current = String::new();
    for ch in numeric.chars() {
        if ch.is_ascii_digit() {
            current.push(ch);
        } else if is_number_separator(ch) {
            if current.is_empty() {
                return Vec::new();
            }
            groups.push(std::mem::take(&mut current));
            separators.push(ch);
        } else {
            return Vec::new();
        }
    }
    if current.is_empty() {
        return Vec::new();
    }
    groups.push(current);

    let grouped_integer = groups.concat();
    let integer_reading = if separators.is_empty() {
        true
    } else {
        let first_separator = separators[0];
        separators
            .iter()
            .all(|separator| *separator == first_separator)
            && (1..=3).contains(&groups[0].len())
            && groups[1..].iter().all(|group| group.len() == 3)
    };

    let decimal_reading = separators.last().is_some_and(|last_separator| {
        if !matches!(last_separator, '.' | ',') {
            return false;
        }
        if separators.len() == 1 {
            return true;
        }
        let earlier_separators = &separators[..separators.len() - 1];
        let grouping_separator = earlier_separators[0];
        grouping_separator != *last_separator
            && earlier_separators
                .iter()
                .all(|separator| *separator == grouping_separator)
            && (1..=3).contains(&groups[0].len())
            && groups[1..groups.len() - 1]
                .iter()
                .all(|group| group.len() == 3)
    });

    let mut forms = Vec::new();
    if integer_reading {
        forms.push(format!("{sign}{grouped_integer}{percent}"));
    }
    if decimal_reading {
        let integer = groups[..groups.len() - 1].concat();
        let fractional = &groups[groups.len() - 1];
        let decimal = format!("{sign}{integer}.{fractional}{percent}");
        if !forms.contains(&decimal) {
            forms.push(decimal);
        }
    }
    forms
}

fn is_number_separator(ch: char) -> bool {
    matches!(ch, '.' | ',' | ' ' | '\u{00a0}' | '\u{202f}' | '\u{2009}')
}

fn numeric_candidate_ranges(text: &str) -> Vec<(usize, usize, String)> {
    let chars = text.char_indices().collect::<Vec<_>>();
    let mut candidates = Vec::new();
    let mut index = 0;

    while index < chars.len() {
        if !chars[index].1.is_ascii_digit() {
            index += 1;
            continue;
        }

        let start_index = if index > 0
            && matches!(normalize_number_sign(chars[index - 1].1), '-' | '+')
            && (index == 1 || !chars[index - 2].1.is_alphanumeric())
        {
            index - 1
        } else {
            index
        };
        let mut end_index = index + 1;
        while end_index < chars.len() {
            let ch = chars[end_index].1;
            if ch.is_ascii_digit()
                || (matches!(ch, '.' | ',')
                    && chars
                        .get(end_index + 1)
                        .is_some_and(|(_, next)| next.is_ascii_digit()))
                || (is_space_grouping_separator(ch)
                    && has_strict_three_digit_group(&chars, end_index))
            {
                end_index += 1;
            } else {
                break;
            }
        }
        if chars
            .get(end_index)
            .is_some_and(|(_, trailing)| *trailing == '%')
        {
            end_index += 1;
        }

        let start = chars[start_index].0;
        let end = chars
            .get(end_index)
            .map_or(text.len(), |(offset, _)| *offset);
        candidates.push((start, end, normalize_number_signs(&text[start..end])));
        index = end_index;
    }

    candidates
}

fn is_space_grouping_separator(ch: char) -> bool {
    matches!(ch, ' ' | '\u{00a0}' | '\u{202f}' | '\u{2009}')
}

fn has_strict_three_digit_group(chars: &[(usize, char)], separator_index: usize) -> bool {
    (1..=3).all(|offset| {
        chars
            .get(separator_index + offset)
            .is_some_and(|(_, ch)| ch.is_ascii_digit())
    }) && chars
        .get(separator_index + 4)
        .is_none_or(|(_, ch)| !ch.is_ascii_digit())
}

/// Numeric substrings of `text`, including space-grouped forms such as `90 000 000`.
fn numeric_candidates(text: &str) -> Vec<String> {
    numeric_candidate_ranges(text)
        .into_iter()
        .map(|(_, _, candidate)| candidate)
        .collect()
}

/// Locale-insensitive presence check for spans that contain numbers.
fn numeric_span_present(span: &str, translation: &str) -> bool {
    let span_ranges = numeric_candidate_ranges(span);
    let span_numbers = span_ranges
        .iter()
        .map(|(_, _, candidate)| candidate.clone())
        .collect::<Vec<_>>();
    if span_numbers.is_empty() {
        return false;
    }
    let starts_with_number = span_ranges.first().is_some_and(|(start, _, _)| *start == 0);

    let mut translated_number_forms = numeric_candidates(translation)
        .iter()
        .map(|candidate| canonical_number_forms(candidate))
        .collect::<Vec<_>>();
    for span_number in span_numbers {
        let required_forms = canonical_number_forms(&span_number);
        if required_forms.is_empty() {
            return false;
        }
        let Some(match_index) = translated_number_forms.iter().position(|candidate_forms| {
            required_forms
                .iter()
                .any(|required| candidate_forms.contains(required))
        }) else {
            return false;
        };
        translated_number_forms.remove(match_index);
    }

    let mut literal_remainder = String::with_capacity(span.len());
    let mut copied_until = 0;
    for (start, end, _) in span_ranges {
        literal_remainder.push_str(&span[copied_until..start]);
        literal_remainder.push(' ');
        copied_until = end;
    }
    literal_remainder.push_str(&span[copied_until..]);
    literal_remainder
        .split_whitespace()
        .filter(|token| !starts_with_number || !token.chars().any(char::is_alphabetic))
        .all(|token| exact_protected_span_present(token, translation))
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

fn dangling_numeric_span(value: &str) -> bool {
    let normalized = normalize_number_signs(value);
    let trimmed = normalized.trim_matches(|ch: char| {
        matches!(
            ch,
            ',' | ';' | ':' | '.' | '!' | '?' | '(' | ')' | '[' | ']' | '"' | '\''
        )
    });
    trimmed.ends_with('-') && trimmed.chars().any(|ch| ch.is_ascii_digit())
}

fn compact_numeric_punctuation_span(value: &str) -> Option<String> {
    let normalized = normalize_number_signs(value);
    let trimmed = normalized.trim_matches(|ch: char| {
        matches!(
            ch,
            ',' | ';' | ':' | '.' | '!' | '?' | '(' | ')' | '[' | ']' | '"' | '\''
        )
    });
    let digits = trimmed.chars().filter(|ch| ch.is_ascii_digit()).count();
    if digits < 2 {
        return None;
    }
    if !trimmed.chars().all(|ch| {
        ch.is_ascii_digit()
            || ch.is_ascii_whitespace()
            || matches!(
                ch,
                '.' | ',' | ';' | ':' | '/' | '-' | '+' | '%' | '$' | '(' | ')'
            )
    }) {
        return None;
    }
    Some(compact_ascii_whitespace(trimmed))
}

fn compact_ascii_whitespace(value: &str) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace())
        .collect()
}

fn canonical_decimal_number(value: &str) -> Option<String> {
    let normalized = normalize_number_signs(value);
    let trimmed = normalized.trim_matches(|ch: char| {
        matches!(
            ch,
            ',' | ';' | ':' | '.' | '!' | '?' | '(' | ')' | '[' | ']' | '"' | '\''
        )
    });
    if trimmed.is_empty() || !trimmed.chars().any(|ch| ch.is_ascii_digit()) {
        return None;
    }
    let percent = trimmed.ends_with('%');
    let numeric = trimmed.strip_suffix('%').unwrap_or(trimmed);
    if !numeric
        .chars()
        .all(|ch| ch.is_ascii_digit() || matches!(ch, '.' | ',' | '-' | '+'))
    {
        return None;
    }
    if numeric.matches('.').count() + numeric.matches(',').count() > 1 {
        return None;
    }
    let separator = numeric.find('.').or_else(|| numeric.find(','));
    let mut canonical = match separator {
        Some(index) => {
            let (whole, fractional_with_separator) = numeric.split_at(index);
            let fractional = &fractional_with_separator[1..];
            if whole.is_empty()
                || fractional.is_empty()
                || !whole
                    .trim_start_matches(['-', '+'])
                    .chars()
                    .all(|ch| ch.is_ascii_digit())
                || !fractional.chars().all(|ch| ch.is_ascii_digit())
            {
                return None;
            }
            format!("{whole}.{fractional}")
        }
        None => numeric.to_string(),
    };
    if percent {
        canonical.push('%');
    }
    Some(canonical)
}

fn numeric_runs(text: &str) -> Vec<String> {
    let mut runs = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_ascii_digit() || matches!(ch, '.' | ',' | '-' | '+' | '%' | '−' | '–' | '—')
        {
            current.push(normalize_number_sign(ch));
        } else if !current.is_empty() {
            runs.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        runs.push(current);
    }
    runs
}

fn normalize_number_signs(value: &str) -> String {
    value.chars().map(normalize_number_sign).collect()
}

fn normalize_number_sign(ch: char) -> char {
    match ch {
        '−' | '–' | '—' => '-',
        _ => ch,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn friendly_validation_error_maps_raw_serde_phrases_to_sentences() {
        // (raw message, expected category, substring that must appear in the
        // friendly sentence). The dogfood example is the first entry: raw
        // serde internals must never leak into persisted text.
        let mapping: &[(&str, InvalidResponseCategory, &str)] = &[
            (
                "invalid batch JSON: missing field `translation` at line 1 column 157",
                InvalidResponseCategory::MissingField,
                "missing a required field",
            ),
            (
                "invalid batch JSON: EOF while parsing a value at line 1 column 88",
                InvalidResponseCategory::InvalidJson,
                "malformed JSON",
            ),
            (
                "JSON parse failed after 3 attempts: trailing characters at line 2 column 1",
                InvalidResponseCategory::InvalidJson,
                "malformed JSON",
            ),
            (
                "batch response incomplete: requested 5 items, returned 3; missing item IDs: a, b",
                InvalidResponseCategory::MarkerMismatch,
                "requested set of items",
            ),
            (
                "error: inline marker missing: m1",
                InvalidResponseCategory::MarkerMismatch,
                "required structure",
            ),
            (
                "error: protected span missing: https://example.com [kind=url]",
                InvalidResponseCategory::MarkerMismatch,
                "protected spans",
            ),
            (
                "duplicate item ID in batch response",
                InvalidResponseCategory::MarkerMismatch,
                "required structure",
            ),
            (
                "batch output was truncated: max_output_tokens limit reached",
                InvalidResponseCategory::Unknown,
                "cut off before it finished",
            ),
            (
                "the model produced no content after 4096 output tokens: it exhausted its \
                 output budget before answering.",
                InvalidResponseCategory::Unknown,
                "no usable content",
            ),
            (
                "some totally unfamiliar failure",
                InvalidResponseCategory::Unknown,
                "could not be used as-is",
            ),
        ];
        for (raw, category, expected_substring) in mapping {
            assert_eq!(
                invalid_response_category(raw),
                *category,
                "category mismatch for raw: {raw}"
            );
            let friendly = friendly_validation_error(raw);
            assert!(
                friendly.contains(expected_substring),
                "friendly text for {raw:?} should mention {expected_substring:?}, got: {friendly}"
            );
        }
    }

    #[test]
    fn friendly_validation_error_never_leaks_raw_serde_internals() {
        for raw in [
            "invalid batch JSON: missing field `translation` at line 1 column 157",
            "invalid batch JSON: EOF while parsing an object at line 1 column 88",
            "JSON parse failed after 3 attempts: trailing characters at line 2 column 1",
        ] {
            let friendly = friendly_validation_error(raw);
            for leak in ["line 1", "column", "`translation`", "invalid batch JSON"] {
                assert!(
                    !friendly.contains(leak),
                    "friendly text must not leak {leak:?}: {friendly}"
                );
            }
        }
    }

    #[test]
    fn friendly_validation_error_sentences_are_stable() {
        // The sentences are a contract for reports and dashboards; pin them.
        assert_eq!(
            friendly_validation_error("invalid batch JSON: missing field `id` at line 1 column 4"),
            "The model's response was missing a required field for one or more blocks \
             — the batch will be retried or repaired."
        );
        assert_eq!(
            friendly_validation_error(
                "batch output was truncated: max_output_tokens limit reached"
            ),
            "The model's response was cut off before it finished — the output-token \
             budget was exhausted; the batch will be retried with a larger budget or \
             split into smaller pieces."
        );
    }
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
    fn allows_unchanged_nonlinguistic_blocks() {
        for text in [
            "9780795707711",
            "9780795707711-2",
            "* * *",
            "1983",
            "<m1>1</m1>",
        ] {
            assert!(
                source_copy_validation_error(text, text, None).is_none(),
                "{text}"
            );
        }
        // A numerical prefix must not exempt the prose that follows it.
        let prose = "1983: The revolution begins";
        assert!(source_copy_validation_error(prose, prose, None).is_some());
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
        assert!(source_copy_validation_error("A short title", "A short title", None).is_some());
    }

    #[test]
    fn allows_reference_sections() {
        assert!(source_copy_validation_error(SOURCE, SOURCE, Some("Endnotes")).is_none());
        assert!(source_copy_validation_error(SOURCE, SOURCE, Some("Bibliography")).is_none());
    }

    /// Unspaced prose of a length the detector is meant to cover. `words`
    /// splits on non-alphanumerics and Han characters are alphanumeric, so
    /// each of these clauses is one "word" -- far below the word floor the
    /// English paragraph above clears easily.
    const CJK_SOURCE: &str = "矛盾的普遍性和特殊性的关系是矛盾问题的精髓，\
        不懂得它就等于抛弃了辩证法。事物的性质主要地是由取得支配地位的矛盾的主要方面所规定的，\
        取得支配地位的矛盾的主要方面起了变化，事物的性质也就随着变化。\
        然而这种情形不是固定的，矛盾的主要和非主要的方面互相转化着，事物的性质也就随着起变化。";

    #[test]
    fn rejects_nearly_complete_source_copy_in_an_unspaced_script() {
        // The same defect as `rejects_nearly_complete_source_copy`: a provider
        // echoed the source back with a token changed rather than translating.
        let translation = CJK_SOURCE.replace("精髓", "核心");
        let error = source_copy_validation_error(CJK_SOURCE, &translation, Some("第一章"));
        assert!(
            error
                .as_deref()
                .is_some_and(|message| message.contains("retains")),
            "an unspaced near-copy must be caught like a spaced one, got {error:?}"
        );
    }

    #[test]
    fn allows_a_real_translation_of_unspaced_source() {
        assert!(
            source_copy_validation_error(
                CJK_SOURCE,
                "Il rapporto fra universalità e particolarità della contraddizione è \
                 l'essenza del problema della contraddizione, e non comprenderlo equivale \
                 ad abbandonare la dialettica. La natura di una cosa è determinata \
                 principalmente dall'aspetto principale della contraddizione dominante.",
                Some("第一章")
            )
            .is_none(),
            "a genuine translation must not be flagged"
        );
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

    #[test]
    fn protected_span_presence_accepts_localized_numeric_forms() {
        assert!(protected_span_present("0.1", "diametro da 0,1 a 1 mm"));
        assert!(protected_span_present(
            "-63.5",
            "il potenziale era circa –63,5 mV"
        ));
        assert!(protected_span_present(
            "1957,1989",
            "Skou (1957, 1989) isolò una ATPasi"
        ));
        assert!(protected_span_present("10-", "7,3 × 10⁻⁷ mol cm⁻²"));
        assert!(protected_span_present(
            "1,000.50",
            "Il totale era 1.000,50."
        ));
        assert!(protected_span_present("4th", "il 4º tentativo"));
        assert!(protected_span_present(
            "19-August",
            "dal 19 luglio al 7 agosto"
        ));
        assert!(protected_span_present(
            "1857-1918)—tsarist",
            "(1857-1918)—generale zarista"
        ));
        assert!(!protected_span_present("4th", "il 5º tentativo"));
        assert!(!protected_span_present(
            "19-August",
            "dal 20 luglio al 7 agosto"
        ));
        assert!(!protected_span_present(
            "1857-1918)—tsarist",
            "(1857-1919)—generale zarista"
        ));
    }

    #[test]
    fn protected_span_presence_accepts_localized_statistical_decimals() {
        assert!(protected_span_present(
            "p < 0.05",
            "Il risultato era p < 0,05."
        ));
        assert!(protected_span_present(
            "F = 3.86",
            "Il valore osservato era F = 3,86."
        ));
    }

    #[test]
    fn protected_span_presence_accepts_localized_digit_grouping() {
        assert!(protected_span_present(
            "90,000,000",
            "La popolazione raggiunse 90.000.000 di persone."
        ));
        assert!(protected_span_present(
            "90,000,000",
            "La popolazione raggiunse 90 000 000 di persone."
        ));
        assert!(protected_span_present(
            "1,000,000,000",
            "Il totale era 1.000.000.000."
        ));
    }

    #[test]
    fn protected_span_presence_rejects_missing_or_wrong_statistical_number() {
        assert!(!protected_span_present(
            "p < 0.05",
            "Il risultato non conteneva alcun valore numerico."
        ));
        assert!(!protected_span_present(
            "p < 0.05",
            "Il risultato era p < 0,06."
        ));
        assert!(!protected_span_present(
            "p < 0.05",
            "Il risultato era 0,05."
        ));
    }

    #[test]
    fn protected_span_presence_still_rejects_absent_numbers() {
        assert!(!protected_span_present(
            "5.16",
            "Si noti che questa forma di rettificazione deriva dai canali aperti."
        ));
    }

    #[test]
    fn marker_reference_text_rejects_dropped_token() {
        let source = "Parola.<m0><m1>*2</m1></m0> Segue.";
        let translation = "Parola.<m0><m1></m1></m0> Segue.";

        assert_eq!(
            marker_reference_text_error(source, translation).as_deref(),
            Some("inline marker m1 lost its reference text '*2'")
        );
    }

    #[test]
    fn marker_reference_text_accepts_preserved_token() {
        let source = "Parola.<m0><m1>*2</m1></m0> Segue.";
        let translation = "Parola.<m0><m1>*2</m1></m0> Continua.";

        assert_eq!(marker_reference_text_error(source, translation), None);
    }

    #[test]
    fn marker_reference_text_supports_non_ascii_numbering() {
        let source = "Parola.<m1>١٢</m1> Segue.";
        let preserved = "Parola.<m1>١٢</m1> Continua.";
        let dropped = "Parola.<m1></m1> Continua.";

        assert_eq!(marker_reference_text_error(source, preserved), None);
        assert!(
            marker_reference_text_error(source, dropped)
                .is_some_and(|error| error.contains("lost its reference text"))
        );
    }

    #[test]
    fn marker_reference_text_allows_prose_to_change() {
        let source = "Una <m0>beautiful</m0> giornata";
        let translation = "Una <m0>bellissima</m0> giornata";

        assert_eq!(marker_reference_text_error(source, translation), None);
    }

    #[test]
    fn toki_pona_gate_rejects_source_leakage_and_invented_words() {
        assert!(
            target_language_validation_error(
                "Toki Pona",
                "Il mondo della nostra societa non cambia.",
                "jan li toki e ni. il mondo della nostra societa non cambia.",
                &[],
            )
            .is_some_and(|error| error.contains("Italian"))
        );
        assert!(
            target_language_validation_error(
                "Toki Pona",
                "La biotecnologia cambia.",
                "jan li toki e bioteknoloji.",
                &[],
            )
            .is_some_and(|error| error.contains("bioteknoloji"))
        );
    }

    #[test]
    fn toki_pona_gate_allows_core_words_names_and_protected_foreign_text() {
        assert_eq!(
            target_language_validation_error(
                "Toki Pona",
                "Roberto parla dell'Italia usando https://example.com.",
                "jan Lopeto li toki e nimi Italia lon lipu https://example.com.",
                &["https://example.com".to_string()],
            ),
            None
        );
        assert_eq!(
            target_language_validation_error(
                "Toki Pona",
                "See https://ourworldindata.org/stages-of-growth and https://europa.eu",
                "lipu ourworldindata en lipu europa eu li toki e ni.",
                &[],
            ),
            None
        );
    }

    #[test]
    fn toki_pona_gate_rejects_known_grammar_and_repetition_failures() {
        for invalid in [
            "mi li jan pona.",
            "mi lukin e jan en soweli.",
            "jan li toki e luka luka luka.",
        ] {
            assert!(
                target_language_validation_error("Toki Pona", "", invalid, &[]).is_some(),
                "expected strict Toki Pona rejection for {invalid:?}"
            );
        }
    }

    #[test]
    fn toki_pona_gate_allows_acronyms_protected_labels_and_roman_numerals() {
        assert_eq!(
            target_language_validation_error(
                "Toki Pona",
                "ISBN 123, CO e sapiens, capitolo IX",
                "ISBN 123 li toki e CO e sapiens lon lipu nanpa IX.",
                &["123".to_string(), "sapiens".to_string()],
            ),
            None
        );
        assert!(
            target_language_validation_error(
                "Toki Pona",
                "capitolo IX",
                "lipu nanpa IX li toki e PIJETA.",
                &[],
            )
            .is_some_and(|error| error.contains("PIJETA"))
        );
    }

    #[test]
    fn toki_pona_gate_does_not_allow_untranslated_lowercase_source_prose() {
        let source = "The latest outlook projects that world energy consumption will grow.";
        assert!(
            target_language_validation_error("Toki Pona", source, source, &[])
                .is_some_and(|error| error.contains("unapproved lowercase word"))
        );
    }

    #[test]
    fn toki_pona_gate_rejects_capitalized_foreign_words_without_a_name_head() {
        assert!(
            target_language_validation_error(
                "Toki Pona",
                "Energia pulita.",
                "Energia li pona.",
                &[],
            )
            .is_some_and(|error| error.contains("Energia"))
        );
    }

    #[test]
    fn toki_pona_gate_rejects_errors_instead_of_rewriting_meaning() {
        for invalid in [
            "mi li pona.",
            "ijo pi pona li lon.",
            "jan li toki e luka luka luka.",
            "ma italia li lon.",
        ] {
            assert!(
                target_language_validation_error("Toki Pona", "", invalid, &[]).is_some(),
                "invalid model output must be retried, not rewritten: {invalid:?}"
            );
        }

        assert_eq!(
            target_language_validation_error(
                "Toki Pona",
                "OPEC e U",
                "kulupu OPEC li toki e U.",
                &[],
            ),
            None
        );
    }
}
