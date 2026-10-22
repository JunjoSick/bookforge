//! Language names for prompts.
//!
//! A run's target is usually given as a code such as `it`, and prompts used to
//! say "Translate from the source language to it." A bare code is ambiguous to
//! a model (`it` reads as a pronoun): on one book DeepSeek returned whole
//! batches untranslated, deterministically, until the prompt named Italian.
//! Prompts therefore name a known ISO 639-1 code and keep the code beside it;
//! anything else, such as a full name like `Toki Pona`, is used as given.

const ISO_639_1_NAMES: &[(&str, &str)] = &[
    ("af", "Afrikaans"),
    ("ar", "Arabic"),
    ("bg", "Bulgarian"),
    ("bn", "Bengali"),
    ("ca", "Catalan"),
    ("cs", "Czech"),
    ("cy", "Welsh"),
    ("da", "Danish"),
    ("de", "German"),
    ("el", "Greek"),
    ("en", "English"),
    ("eo", "Esperanto"),
    ("es", "Spanish"),
    ("et", "Estonian"),
    ("eu", "Basque"),
    ("fa", "Persian"),
    ("fi", "Finnish"),
    ("fr", "French"),
    ("ga", "Irish"),
    ("gl", "Galician"),
    ("he", "Hebrew"),
    ("hi", "Hindi"),
    ("hr", "Croatian"),
    ("hu", "Hungarian"),
    ("hy", "Armenian"),
    ("id", "Indonesian"),
    ("is", "Icelandic"),
    ("it", "Italian"),
    ("ja", "Japanese"),
    ("ka", "Georgian"),
    ("ko", "Korean"),
    ("la", "Latin"),
    ("lt", "Lithuanian"),
    ("lv", "Latvian"),
    ("mk", "Macedonian"),
    ("ms", "Malay"),
    ("mt", "Maltese"),
    ("nb", "Norwegian Bokmål"),
    ("nl", "Dutch"),
    ("nn", "Norwegian Nynorsk"),
    ("no", "Norwegian"),
    ("pl", "Polish"),
    ("pt", "Portuguese"),
    ("ro", "Romanian"),
    ("ru", "Russian"),
    ("sk", "Slovak"),
    ("sl", "Slovenian"),
    ("sq", "Albanian"),
    ("sr", "Serbian"),
    ("sv", "Swedish"),
    ("sw", "Swahili"),
    ("ta", "Tamil"),
    ("th", "Thai"),
    ("tl", "Tagalog"),
    ("tr", "Turkish"),
    ("uk", "Ukrainian"),
    ("ur", "Urdu"),
    ("vi", "Vietnamese"),
    ("zh", "Chinese"),
];

/// The language as a prompt should name it: `Italian (it)` for a known
/// ISO 639-1 code, also with a region or script subtag (`pt-BR` becomes
/// `Portuguese (pt-BR)`); any other value trimmed but otherwise unchanged.
pub fn prompt_language_name(language: &str) -> String {
    let language = language.trim();
    let primary = language.split(['-', '_']).next().unwrap_or(language);
    match ISO_639_1_NAMES
        .iter()
        .find(|(code, _)| primary.eq_ignore_ascii_case(code))
    {
        Some((_, name)) => format!("{name} ({language})"),
        None => language.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_codes_are_named_with_the_code_kept() {
        assert_eq!(prompt_language_name("it"), "Italian (it)");
        assert_eq!(prompt_language_name(" EN "), "English (EN)");
        assert_eq!(prompt_language_name("pt-BR"), "Portuguese (pt-BR)");
        assert_eq!(prompt_language_name("zh_Hant"), "Chinese (zh_Hant)");
    }

    #[test]
    fn names_and_unknown_values_are_used_as_given() {
        assert_eq!(prompt_language_name("Italian"), "Italian");
        assert_eq!(prompt_language_name("Toki Pona"), "Toki Pona");
        assert_eq!(prompt_language_name("xx"), "xx");
        assert_eq!(prompt_language_name(""), "");
    }
}
