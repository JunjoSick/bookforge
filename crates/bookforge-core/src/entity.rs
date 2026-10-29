//! Named entities — a structured extension of the glossary for
//! characters, places, and named items whose grammatical gender matters
//! in the target language.
//!
//! Glossary terms enforce a source → target substitution; entities go
//! further and tell the model how to inflect adjectives and articles
//! around the translated name. The rendered block reads like a
//! grammatical-agreement table (see [`render_entity_agreement_block`]),
//! so the model can keep gender concord consistent across paragraphs.
//!
//! Italian, French, Spanish, German, etc. all need this; English
//! doesn't. The feature is purely additive — empty input produces an
//! empty block.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::glossary::GlossaryScopeKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntityGender {
    #[serde(rename = "m")]
    Masculine,
    #[serde(rename = "f")]
    Feminine,
    #[serde(rename = "n")]
    Neuter,
}

impl EntityGender {
    pub fn as_label(self) -> &'static str {
        match self {
            EntityGender::Masculine => "masculine",
            EntityGender::Feminine => "feminine",
            EntityGender::Neuter => "neuter",
        }
    }

    pub fn as_short(self) -> &'static str {
        match self {
            EntityGender::Masculine => "m",
            EntityGender::Feminine => "f",
            EntityGender::Neuter => "n",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    pub scope_kind: GlossaryScopeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_id: Option<String>,
    pub source_name: String,
    pub target_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gender_target: Option<EntityGender>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    pub source_language: String,
    pub target_language: String,
}

/// Merge entities across scopes with the same `book > series > global`
/// precedence as glossary terms. Entities are keyed on
/// `(source_name, source_language, target_language)`; the highest-
/// priority row wins.
pub fn merge_scope_entities(entities: &[Entity]) -> Vec<Entity> {
    let mut by_key: HashMap<(String, String, String), Entity> = HashMap::new();
    for entity in entities {
        let key = (
            entity.source_name.clone(),
            entity.source_language.clone(),
            entity.target_language.clone(),
        );
        match by_key.get(&key) {
            Some(existing) if existing.scope_kind.priority() > entity.scope_kind.priority() => {}
            _ => {
                by_key.insert(key, entity.clone());
            }
        }
    }
    let mut merged: Vec<Entity> = by_key.into_values().collect();
    merged.sort_by(|a, b| {
        a.source_language
            .cmp(&b.source_language)
            .then_with(|| a.target_language.cmp(&b.target_language))
            .then_with(|| a.source_name.cmp(&b.source_name))
    });
    merged
}

/// Render merged entities as a grammatical-agreement block. Empty input
/// returns an empty string so the placeholder substitutes to nothing in
/// templates that don't reference the table.
pub fn render_entity_agreement_block(entities: &[Entity]) -> String {
    if entities.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "=== Entity grammatical agreement (use this for adjective/article concord) ===\n",
    );
    for entity in entities {
        let mut line = format!("- {}", entity.target_name);
        if entity.target_name != entity.source_name {
            line.push_str(&format!(" ({})", entity.source_name));
        }
        if let Some(gender) = entity.gender_target {
            line.push_str(&format!(": {}", gender.as_label()));
        } else {
            line.push_str(": unspecified");
        }
        if let Some(role) = entity.role.as_deref().filter(|r| !r.is_empty()) {
            line.push_str(&format!(" [{role}]"));
        }
        out.push_str(&line);
        out.push('\n');
        // Notes stay attached to their entity: continuation lines are
        // indented under the first so a multi-line note cannot read as a
        // new table row.
        let mut note_lines = entity
            .notes
            .as_deref()
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .filter(|note| !note.is_empty());
        if let Some(first) = note_lines.next() {
            out.push_str("  Notes: ");
            out.push_str(first);
            out.push('\n');
            for note in note_lines {
                out.push_str("         ");
                out.push_str(note);
                out.push('\n');
            }
        }
    }
    out.push_str("=== End ===\n");
    out
}

fn has_note_guidance(entity: &Entity) -> bool {
    entity
        .notes
        .as_deref()
        .is_some_and(|notes| !notes.trim().is_empty())
}

/// Stable fingerprint of a merged entity set. Empty input still produces
/// a stable fingerprint so the cache namespace can ignore-or-include
/// uniformly.
pub fn entities_fingerprint(entities: &[Entity]) -> String {
    let mut normalized: Vec<Entity> = entities.to_vec();
    // Strip ids and sort for stability — the same logical set must
    // fingerprint identically regardless of insertion order.
    for entity in &mut normalized {
        entity.id = None;
    }
    normalized.sort_by(|a, b| {
        a.scope_kind
            .priority()
            .cmp(&b.scope_kind.priority())
            .then_with(|| a.scope_id.cmp(&b.scope_id))
            .then_with(|| a.source_language.cmp(&b.source_language))
            .then_with(|| a.target_language.cmp(&b.target_language))
            .then_with(|| a.source_name.cmp(&b.source_name))
            .then_with(|| a.target_name.cmp(&b.target_name))
    });
    // Notes were always part of this payload, but earlier prompts dropped
    // them. Version only the sets whose notes now add guidance, so their old
    // cache entries are not reused while every other namespace is unchanged.
    let schema = if normalized.iter().any(has_note_guidance) {
        2
    } else {
        1
    };
    let payload = serde_json::json!({
        "schema": schema,
        "entities": normalized,
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

    fn entity(name: &str, target: &str, scope: GlossaryScopeKind, gender: EntityGender) -> Entity {
        Entity {
            id: None,
            scope_kind: scope,
            scope_id: Some("test".to_string()),
            source_name: name.to_string(),
            target_name: target.to_string(),
            gender_target: Some(gender),
            role: None,
            notes: None,
            source_language: "English".to_string(),
            target_language: "Italian".to_string(),
        }
    }

    #[test]
    fn render_block_returns_empty_for_no_entities() {
        assert_eq!(render_entity_agreement_block(&[]), "");
    }

    #[test]
    fn render_block_includes_source_when_target_differs() {
        let entities = vec![entity(
            "the Ring",
            "l'Anello",
            GlossaryScopeKind::Book,
            EntityGender::Masculine,
        )];
        let rendered = render_entity_agreement_block(&entities);
        assert!(rendered.contains("l'Anello (the Ring): masculine"));
    }

    #[test]
    fn render_block_omits_source_when_target_matches() {
        let entities = vec![entity(
            "Galadriel",
            "Galadriel",
            GlossaryScopeKind::Book,
            EntityGender::Feminine,
        )];
        let rendered = render_entity_agreement_block(&entities);
        assert!(rendered.contains("- Galadriel: feminine"));
        assert!(!rendered.contains("(Galadriel)"));
    }

    #[test]
    fn merge_book_overrides_series_overrides_global() {
        let global = entity(
            "Aragorn",
            "Aragorn-old",
            GlossaryScopeKind::Global,
            EntityGender::Masculine,
        );
        let series = entity(
            "Aragorn",
            "Aragorn-series",
            GlossaryScopeKind::Series,
            EntityGender::Masculine,
        );
        let book = entity(
            "Aragorn",
            "Aragorn",
            GlossaryScopeKind::Book,
            EntityGender::Masculine,
        );
        let merged = merge_scope_entities(&[global, series, book]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].target_name, "Aragorn");
        assert_eq!(merged[0].scope_kind, GlossaryScopeKind::Book);
    }

    #[test]
    fn merge_keeps_distinct_source_names() {
        let a = entity(
            "Galadriel",
            "Galadriel",
            GlossaryScopeKind::Book,
            EntityGender::Feminine,
        );
        let b = entity(
            "Boromir",
            "Boromir",
            GlossaryScopeKind::Book,
            EntityGender::Masculine,
        );
        let merged = merge_scope_entities(&[a, b]);
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn entities_fingerprint_is_stable_across_input_order() {
        let a = entity(
            "Galadriel",
            "Galadriel",
            GlossaryScopeKind::Book,
            EntityGender::Feminine,
        );
        let b = entity(
            "Boromir",
            "Boromir",
            GlossaryScopeKind::Book,
            EntityGender::Masculine,
        );
        let fp_ab = entities_fingerprint(&[a.clone(), b.clone()]);
        let fp_ba = entities_fingerprint(&[b, a]);
        assert_eq!(fp_ab, fp_ba);
    }

    #[test]
    fn entities_fingerprint_changes_when_gender_changes() {
        let masc = entity("X", "X", GlossaryScopeKind::Book, EntityGender::Masculine);
        let fem = entity("X", "X", GlossaryScopeKind::Book, EntityGender::Feminine);
        assert_ne!(entities_fingerprint(&[masc]), entities_fingerprint(&[fem]));
    }

    #[test]
    fn entities_fingerprint_of_empty_is_stable() {
        let a = entities_fingerprint(&[]);
        let b = entities_fingerprint(&[]);
        assert_eq!(a, b);
        assert!(!a.is_empty());
    }

    #[test]
    fn render_block_keeps_multiline_notes_with_their_entity() {
        let mut captain = entity(
            "Mira Voss",
            "Mira Voss",
            GlossaryScopeKind::Book,
            EntityGender::Feminine,
        );
        captain.notes =
            Some("  Address her as Capitana Voss.\n\n  She speaks in clipped sentences.  ".into());
        let ship = entity(
            "the Gull",
            "la Gabbiana",
            GlossaryScopeKind::Book,
            EntityGender::Feminine,
        );
        let rendered = render_entity_agreement_block(&[captain, ship]);
        assert!(
            rendered.contains(
                "- Mira Voss: feminine\n  Notes: Address her as Capitana Voss.\n         She speaks in clipped sentences.\n- la Gabbiana (the Gull): feminine\n"
            ),
            "{rendered}"
        );
    }

    #[test]
    fn entity_notes_invalidate_legacy_cache_without_changing_unaffected_namespaces() {
        let mut captain = entity(
            "Mira Voss",
            "Mira Voss",
            GlossaryScopeKind::Book,
            EntityGender::Feminine,
        );
        captain.role = Some("harbor-master".into());
        // Captured from the implementation that left notes out of prompts.
        assert_eq!(
            entities_fingerprint(&[]),
            "9ed90ba863639b8c4a07a1ebbe0e8ad2785605c1657e2d1e455e181a51a834a1"
        );
        assert_eq!(
            entities_fingerprint(std::slice::from_ref(&captain)),
            "c58e04d138dcbe2bffe17d2ce99d34b4dd7a60f3cc74d143b00425eeca0998e6"
        );
        let rendered_without_notes = render_entity_agreement_block(std::slice::from_ref(&captain));

        // Blank notes add no guidance, so neither the prompt nor the cache
        // identity may change.
        captain.notes = Some(" \n\t ".into());
        assert_eq!(
            render_entity_agreement_block(std::slice::from_ref(&captain)),
            rendered_without_notes
        );
        assert_eq!(
            entities_fingerprint(std::slice::from_ref(&captain)),
            "4fdda60b8ed76bf5f3dbd40f7542f64fc317705a2fb36740e6e6aab81db3957d"
        );

        // Real notes now reach the prompt; entries cached when the same notes
        // were silently dropped must not be reused.
        captain.notes = Some("Address her as Capitana Voss.".into());
        assert!(
            render_entity_agreement_block(std::slice::from_ref(&captain))
                .contains("  Notes: Address her as Capitana Voss.\n")
        );
        assert_ne!(
            entities_fingerprint(std::slice::from_ref(&captain)),
            "75d56a142dd12ef5e0be7dac5fa763d36c676bbdf73e093d7509ebade1935b15"
        );
    }

    #[test]
    fn render_block_includes_role_when_present() {
        let mut e = entity(
            "Galadriel",
            "Galadriel",
            GlossaryScopeKind::Book,
            EntityGender::Feminine,
        );
        e.role = Some("elf-queen".to_string());
        let rendered = render_entity_agreement_block(&[e]);
        assert!(rendered.contains("[elf-queen]"));
    }
}
