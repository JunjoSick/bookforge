//! Style sheets — per-book/series/global rendering hints injected into
//! the translation prompt.
//!
//! A style sheet captures the per-book stylistic decisions that
//! `--prompt-extra` was a thin proxy for: narrative register, dialogue
//! formality, loanword policy, and free-form custom instructions. Several
//! sheets at different scopes can be active simultaneously; they merge
//! `book > series > global` with the same precedence as glossary terms
//! (see [`merge_style_sheets`]).
//!
//! The merged sheet renders as a single prompt block; the prompt
//! template references it via `{{style_guide_block}}`. When no sheet is
//! active, the block renders to an empty string.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::glossary::GlossaryScopeKind;

/// Narrative register: maps to source-language → target-language voice
/// conventions. Values are strings so the user can pick conventions
/// outside this enum (`passato_remoto` etc.) without us pre-enumerating
/// every target-language idiom.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterFields {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub narration: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub narration_tense: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dialogue_default: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loanword_policy: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceFields {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub narrator_register: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preserve_anglicisms: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_audience: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gender_of_unspecified_narrator: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoNotFields {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub translate_terms: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preserve_punctuation: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StyleSheet {
    pub scope_kind: GlossaryScopeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_id: Option<String>,
    pub target_language: String,
    #[serde(default)]
    pub register: RegisterFields,
    #[serde(default)]
    pub voice: VoiceFields,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_text_instructions: Option<String>,
    #[serde(default)]
    pub do_not: DoNotFields,
}

/// Merge multiple style sheets following `book > series > global`
/// precedence (mirrors [`crate::glossary::merge_scope_terms`]). For
/// scalar Optional fields, the highest-priority non-`None` wins. For
/// `do_not.translate_terms` and `do_not.preserve_punctuation`, vectors
/// concatenate with deduplication. `free_text_instructions` concatenates
/// global-first → book-last so book-level instructions appear most
/// prominently to the model. Returns `None` if no input sheets carry any
/// content.
pub fn merge_style_sheets(sheets: &[StyleSheet]) -> Option<StyleSheet> {
    if sheets.is_empty() {
        return None;
    }
    let target_language = sheets[0].target_language.clone();
    let mut ordered: Vec<&StyleSheet> = sheets.iter().collect();
    ordered.sort_by_key(|sheet| sheet.scope_kind.priority());

    let mut merged = StyleSheet {
        scope_kind: GlossaryScopeKind::Global,
        scope_id: None,
        target_language,
        register: RegisterFields::default(),
        voice: VoiceFields::default(),
        free_text_instructions: None,
        do_not: DoNotFields::default(),
    };
    let mut instructions: Vec<String> = Vec::new();
    let mut translate_seen: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    let mut punct_seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for sheet in ordered {
        if let Some(value) = sheet.register.narration.as_ref() {
            merged.register.narration = Some(value.clone());
        }
        if let Some(value) = sheet.register.narration_tense.as_ref() {
            merged.register.narration_tense = Some(value.clone());
        }
        if let Some(value) = sheet.register.dialogue_default.as_ref() {
            merged.register.dialogue_default = Some(value.clone());
        }
        if let Some(value) = sheet.register.loanword_policy.as_ref() {
            merged.register.loanword_policy = Some(value.clone());
        }
        if let Some(value) = sheet.voice.narrator_register.as_ref() {
            merged.voice.narrator_register = Some(value.clone());
        }
        if let Some(value) = sheet.voice.preserve_anglicisms {
            merged.voice.preserve_anglicisms = Some(value);
        }
        if let Some(value) = sheet.voice.target_audience.as_ref() {
            merged.voice.target_audience = Some(value.clone());
        }
        if let Some(value) = sheet.voice.gender_of_unspecified_narrator.as_ref() {
            merged.voice.gender_of_unspecified_narrator = Some(value.clone());
        }
        if let Some(value) = sheet
            .free_text_instructions
            .as_ref()
            .filter(|s| !s.trim().is_empty())
        {
            instructions.push(value.trim().to_string());
        }
        for term in &sheet.do_not.translate_terms {
            if translate_seen.insert(term.clone()) {
                merged.do_not.translate_terms.push(term.clone());
            }
        }
        for token in &sheet.do_not.preserve_punctuation {
            if punct_seen.insert(token.clone()) {
                merged.do_not.preserve_punctuation.push(token.clone());
            }
        }
    }
    if !instructions.is_empty() {
        merged.free_text_instructions = Some(instructions.join("\n\n"));
    }
    if has_content(&merged) {
        Some(merged)
    } else {
        None
    }
}

fn has_content(s: &StyleSheet) -> bool {
    let RegisterFields {
        narration,
        narration_tense,
        dialogue_default,
        loanword_policy,
    } = &s.register;
    let VoiceFields {
        narrator_register,
        preserve_anglicisms,
        target_audience,
        gender_of_unspecified_narrator,
    } = &s.voice;
    narration.is_some()
        || narration_tense.is_some()
        || dialogue_default.is_some()
        || loanword_policy.is_some()
        || narrator_register.is_some()
        || preserve_anglicisms.is_some()
        || target_audience.is_some()
        || gender_of_unspecified_narrator.is_some()
        || s.free_text_instructions.is_some()
        || !s.do_not.translate_terms.is_empty()
        || !s.do_not.preserve_punctuation.is_empty()
}

/// Render the merged style sheet as a prompt block. Empty input returns
/// an empty string so the placeholder substitutes to nothing in
/// templates that don't reference style guides.
pub fn render_style_block(merged: Option<&StyleSheet>) -> String {
    let Some(sheet) = merged else {
        return String::new();
    };
    let mut out = String::from("=== Active style guide ===\n");
    if let Some(narration) = &sheet.register.narration {
        let mut line = format!("Register: {narration}");
        if let Some(tense) = &sheet.register.narration_tense {
            line.push_str(&format!(", narration in {tense}"));
        }
        out.push_str(&line);
        out.push_str(".\n");
    } else if let Some(tense) = &sheet.register.narration_tense {
        out.push_str(&format!("Narration tense: {tense}.\n"));
    }
    if let Some(dialogue) = &sheet.register.dialogue_default {
        out.push_str(&format!("Dialogue default: {dialogue}.\n"));
    }
    if let Some(policy) = &sheet.register.loanword_policy {
        out.push_str(&format!("Loanword policy: {policy}.\n"));
    }
    if let Some(voice) = &sheet.voice.narrator_register {
        out.push_str(&format!("Narrator voice: {voice}.\n"));
    }
    if let Some(audience) = &sheet.voice.target_audience {
        out.push_str(&format!("Target audience: {audience}.\n"));
    }
    if let Some(gender) = &sheet.voice.gender_of_unspecified_narrator {
        out.push_str(&format!("Unspecified-narrator gender: {gender}.\n"));
    }
    if let Some(preserve) = sheet.voice.preserve_anglicisms {
        out.push_str(&format!("Preserve anglicisms: {preserve}.\n"));
    }
    if !sheet.do_not.preserve_punctuation.is_empty() {
        out.push_str(&format!(
            "Preserve punctuation: {}.\n",
            sheet.do_not.preserve_punctuation.join(" ")
        ));
    }
    if !sheet.do_not.translate_terms.is_empty() {
        out.push_str(&format!(
            "Do not translate: {}.\n",
            sheet.do_not.translate_terms.join(", ")
        ));
    }
    if let Some(instructions) = sheet
        .free_text_instructions
        .as_ref()
        .filter(|s| !s.trim().is_empty())
    {
        out.push_str("\nCustom instructions:\n");
        out.push_str(instructions.trim());
        out.push('\n');
    }
    out.push_str("=== End style guide ===\n");
    out
}

/// Stable fingerprint of the merged style sheet for cache namespacing
/// and snapshot integrity. When `merged` is `None`, returns the
/// fingerprint of the empty payload — stable across runs and across
/// upgrades, so users without `--style` see no cache invalidation.
pub fn style_fingerprint(merged: Option<&StyleSheet>) -> String {
    let payload = serde_json::json!({
        "schema": 1,
        "merged": merged,
    });
    let serialized = serde_json::to_vec(&payload).unwrap_or_default();
    let digest = Sha256::digest(serialized);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut hex, "{byte:02x}").expect("write to string");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sheet(scope: GlossaryScopeKind, target: &str) -> StyleSheet {
        StyleSheet {
            scope_kind: scope,
            scope_id: Some("test".to_string()),
            target_language: target.to_string(),
            register: RegisterFields::default(),
            voice: VoiceFields::default(),
            free_text_instructions: None,
            do_not: DoNotFields::default(),
        }
    }

    #[test]
    fn merge_returns_none_for_empty_input() {
        assert!(merge_style_sheets(&[]).is_none());
    }

    #[test]
    fn merge_returns_none_when_all_sheets_are_empty() {
        let sheets = vec![sheet(GlossaryScopeKind::Global, "Italian")];
        assert!(merge_style_sheets(&sheets).is_none());
    }

    #[test]
    fn merge_applies_book_over_series_over_global_precedence_for_scalars() {
        let mut global = sheet(GlossaryScopeKind::Global, "Italian");
        global.register.narration = Some("neutral".to_string());
        global.register.dialogue_default = Some("Lei".to_string());
        let mut series = sheet(GlossaryScopeKind::Series, "Italian");
        series.register.narration = Some("literary".to_string());
        let mut book = sheet(GlossaryScopeKind::Book, "Italian");
        book.register.dialogue_default = Some("tu".to_string());

        let merged = merge_style_sheets(&[global, series, book]).expect("merged");
        assert_eq!(merged.register.narration.as_deref(), Some("literary"));
        assert_eq!(merged.register.dialogue_default.as_deref(), Some("tu"));
    }

    #[test]
    fn merge_concatenates_free_text_global_first_book_last() {
        let mut global = sheet(GlossaryScopeKind::Global, "Italian");
        global.free_text_instructions = Some("global hint".to_string());
        let mut book = sheet(GlossaryScopeKind::Book, "Italian");
        book.free_text_instructions = Some("book hint".to_string());

        let merged = merge_style_sheets(&[book, global]).expect("merged");
        let text = merged.free_text_instructions.expect("instructions");
        let g = text.find("global hint").expect("global");
        let b = text.find("book hint").expect("book");
        assert!(g < b, "global instructions render before book-level ones");
    }

    #[test]
    fn merge_dedupes_do_not_translate_terms() {
        let mut global = sheet(GlossaryScopeKind::Global, "Italian");
        global.do_not.translate_terms = vec!["mithril".to_string()];
        let mut book = sheet(GlossaryScopeKind::Book, "Italian");
        book.do_not.translate_terms = vec!["mithril".to_string(), "lembas".to_string()];
        let merged = merge_style_sheets(&[global, book]).expect("merged");
        assert_eq!(merged.do_not.translate_terms.len(), 2);
        assert!(merged.do_not.translate_terms.contains(&"mithril".to_string()));
        assert!(merged.do_not.translate_terms.contains(&"lembas".to_string()));
    }

    #[test]
    fn render_block_returns_empty_when_no_sheet() {
        assert_eq!(render_style_block(None), "");
    }

    #[test]
    fn render_block_skips_unset_fields() {
        let mut s = sheet(GlossaryScopeKind::Book, "Italian");
        s.register.dialogue_default = Some("tu".to_string());
        let rendered = render_style_block(Some(&s));
        assert!(rendered.contains("Dialogue default: tu."));
        assert!(!rendered.contains("Register:"));
        assert!(!rendered.contains("Narrator voice:"));
    }

    #[test]
    fn render_block_includes_free_text_instructions_after_structured_fields() {
        let mut s = sheet(GlossaryScopeKind::Book, "Italian");
        s.register.narration = Some("literary".to_string());
        s.free_text_instructions = Some("Maintain a literary register.".to_string());
        let rendered = render_style_block(Some(&s));
        let reg = rendered.find("Register:").expect("register present");
        let instr = rendered
            .find("Maintain a literary register.")
            .expect("instructions present");
        assert!(reg < instr);
    }

    #[test]
    fn style_fingerprint_is_stable_across_field_order() {
        let mut a = sheet(GlossaryScopeKind::Book, "Italian");
        a.register.narration = Some("literary".to_string());
        a.register.dialogue_default = Some("tu".to_string());
        let mut b = sheet(GlossaryScopeKind::Book, "Italian");
        b.register.dialogue_default = Some("tu".to_string());
        b.register.narration = Some("literary".to_string());
        assert_eq!(style_fingerprint(Some(&a)), style_fingerprint(Some(&b)));
    }

    #[test]
    fn style_fingerprint_of_none_is_stable() {
        let f1 = style_fingerprint(None);
        let f2 = style_fingerprint(None);
        assert_eq!(f1, f2);
        assert_ne!(f1, "");
    }
}
