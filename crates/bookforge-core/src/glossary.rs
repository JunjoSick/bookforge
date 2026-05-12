use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::segment::Segment;

#[cfg_attr(feature = "cli", derive(clap::ValueEnum))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GlossaryScopeKind {
    Global,
    Series,
    Book,
}

impl GlossaryScopeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Series => "series",
            Self::Book => "book",
        }
    }

    pub fn priority(self) -> usize {
        match self {
            Self::Global => 0,
            Self::Series => 1,
            Self::Book => 2,
        }
    }
}

impl std::str::FromStr for GlossaryScopeKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "global" => Ok(Self::Global),
            "series" => Ok(Self::Series),
            "book" => Ok(Self::Book),
            other => Err(format!(
                "invalid glossary scope '{other}'; expected global, series, or book"
            )),
        }
    }
}

impl std::fmt::Display for GlossaryScopeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg_attr(feature = "cli", derive(clap::ValueEnum))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GlossaryCategory {
    Person,
    Place,
    Object,
    Invented,
    Style,
    Phrase,
    Other,
}

impl GlossaryCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Person => "person",
            Self::Place => "place",
            Self::Object => "object",
            Self::Invented => "invented",
            Self::Style => "style",
            Self::Phrase => "phrase",
            Self::Other => "other",
        }
    }

    pub fn is_high_frequency_anchor(self) -> bool {
        matches!(
            self,
            Self::Person | Self::Place | Self::Object | Self::Invented
        )
    }
}

impl std::str::FromStr for GlossaryCategory {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "person" => Ok(Self::Person),
            "place" => Ok(Self::Place),
            "object" => Ok(Self::Object),
            "invented" => Ok(Self::Invented),
            "style" => Ok(Self::Style),
            "phrase" => Ok(Self::Phrase),
            "other" => Ok(Self::Other),
            other => Err(format!(
                "invalid glossary category '{other}'; expected person, place, object, invented, style, phrase, or other"
            )),
        }
    }
}

impl std::fmt::Display for GlossaryCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg_attr(feature = "cli", derive(clap::ValueEnum))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GlossaryStatus {
    UserSeeded,
    AutoCandidate,
    Accepted,
    Rejected,
}

impl GlossaryStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UserSeeded => "user_seeded",
            Self::AutoCandidate => "auto_candidate",
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
        }
    }

    pub fn is_active(self) -> bool {
        matches!(self, Self::UserSeeded | Self::Accepted)
    }
}

impl std::str::FromStr for GlossaryStatus {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "user_seeded" => Ok(Self::UserSeeded),
            "auto_candidate" => Ok(Self::AutoCandidate),
            "accepted" => Ok(Self::Accepted),
            "rejected" => Ok(Self::Rejected),
            other => Err(format!(
                "invalid glossary status '{other}'; expected user_seeded, auto_candidate, accepted, or rejected"
            )),
        }
    }
}

#[cfg_attr(feature = "cli", derive(clap::ValueEnum))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GlossaryFormat {
    Json,
    Prose,
}

impl GlossaryFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Prose => "prose",
        }
    }
}

impl std::fmt::Display for GlossaryFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlossaryTerm {
    pub id: Option<i64>,
    pub scope_kind: GlossaryScopeKind,
    pub scope_id: Option<String>,
    pub source_text: String,
    pub target_text: String,
    pub category: GlossaryCategory,
    pub notes: Option<String>,
    pub case_sensitive: bool,
    pub always_active: bool,
    pub status: GlossaryStatus,
    pub source_language: String,
    pub target_language: String,
    pub source_count: usize,
}

impl GlossaryTerm {
    pub fn active(&self) -> bool {
        self.status.is_active()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlossaryPromptTerm {
    pub source: String,
    pub target: String,
    pub category: GlossaryCategory,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub term_id: Option<i64>,
    pub case_sensitive: bool,
}

impl GlossaryPromptTerm {
    fn from_term(term: &GlossaryTerm) -> Self {
        Self {
            source: term.source_text.clone(),
            target: term.target_text.clone(),
            category: term.category,
            note: term.notes.clone(),
            term_id: term.id,
            case_sensitive: term.case_sensitive,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentGlossarySelections {
    pub entries_by_segment: HashMap<String, Vec<GlossaryPromptTerm>>,
    pub truncated_authoritative_entries: usize,
}

pub fn merge_scope_terms(terms: &[GlossaryTerm]) -> Vec<GlossaryTerm> {
    let mut by_key: HashMap<(String, bool, String, String), GlossaryTerm> = HashMap::new();
    for term in terms.iter().filter(|term| term.active()) {
        let key = (
            if term.case_sensitive {
                term.source_text.clone()
            } else {
                term.source_text.to_lowercase()
            },
            term.case_sensitive,
            term.source_language.clone(),
            term.target_language.clone(),
        );
        match by_key.get(&key) {
            Some(existing) if existing.scope_kind.priority() > term.scope_kind.priority() => {}
            _ => {
                by_key.insert(key, term.clone());
            }
        }
    }
    let mut merged = by_key.into_values().collect::<Vec<_>>();
    merged.sort_by(|a, b| {
        a.scope_kind
            .priority()
            .cmp(&b.scope_kind.priority())
            .then_with(|| a.source_text.cmp(&b.source_text))
            .then_with(|| a.target_text.cmp(&b.target_text))
    });
    merged
}

pub fn select_glossary_for_segments(
    segments: &[Segment],
    terms: &[GlossaryTerm],
    budget_tokens: usize,
) -> SegmentGlossarySelections {
    let terms = merge_scope_terms(terms);
    let computed_counts = source_counts(segments, &terms);
    let high_frequency = high_frequency_anchors(&terms, &computed_counts, 20);
    let mut entries_by_segment = HashMap::new();
    let mut truncated_authoritative_entries = 0usize;

    for (index, segment) in segments.iter().enumerate() {
        let mut selected = Vec::<&GlossaryTerm>::new();
        let mut seen = HashSet::<i64>::new();

        for term in &terms {
            if term_matches(&segment.source.text, term) {
                push_term(&mut selected, &mut seen, term);
            }
        }

        for term in terms.iter().filter(|term| term.always_active) {
            push_term(&mut selected, &mut seen, term);
        }

        let start = index.saturating_sub(5);
        for previous in &segments[start..index] {
            if previous.section_id != segment.section_id {
                continue;
            }
            for term in &terms {
                if term_matches(&previous.source.text, term) {
                    push_term(&mut selected, &mut seen, term);
                }
            }
        }

        for term in &high_frequency {
            push_term(&mut selected, &mut seen, term);
        }

        let (bounded, truncated) = enforce_budget(selected, budget_tokens);
        truncated_authoritative_entries += truncated;
        entries_by_segment.insert(
            segment.id.0.clone(),
            bounded
                .into_iter()
                .map(GlossaryPromptTerm::from_term)
                .collect(),
        );
    }

    SegmentGlossarySelections {
        entries_by_segment,
        truncated_authoritative_entries,
    }
}

pub fn term_matches(text: &str, term: &GlossaryTerm) -> bool {
    if term.source_text.is_empty() {
        return false;
    }
    if term.case_sensitive {
        text.contains(&term.source_text)
    } else {
        text.to_lowercase()
            .contains(&term.source_text.to_lowercase())
    }
}

pub fn target_matches(text: &str, term: &GlossaryTerm) -> bool {
    if term.target_text.is_empty() {
        return false;
    }
    if term.case_sensitive {
        text.contains(&term.target_text)
    } else {
        text.to_lowercase()
            .contains(&term.target_text.to_lowercase())
    }
}

fn push_term<'a>(
    selected: &mut Vec<&'a GlossaryTerm>,
    seen: &mut HashSet<i64>,
    term: &'a GlossaryTerm,
) {
    let synthetic = term.synthetic_id();
    if seen.insert(synthetic) {
        selected.push(term);
    }
}

fn enforce_budget(terms: Vec<&GlossaryTerm>, budget_tokens: usize) -> (Vec<&GlossaryTerm>, usize) {
    let mut used = 0usize;
    let mut kept = Vec::new();
    let mut truncated = 0usize;
    for term in terms {
        let estimate = estimate_prompt_tokens(term);
        if used + estimate <= budget_tokens || kept.is_empty() {
            used += estimate;
            kept.push(term);
        } else if term.status == GlossaryStatus::UserSeeded || term.always_active {
            truncated += 1;
        }
    }
    (kept, truncated)
}

fn estimate_prompt_tokens(term: &GlossaryTerm) -> usize {
    let note = term.notes.as_deref().unwrap_or("");
    let chars = term.source_text.len()
        + term.target_text.len()
        + term.category.as_str().len()
        + note.len()
        + 16;
    chars.div_ceil(3).max(1)
}

fn source_counts(segments: &[Segment], terms: &[GlossaryTerm]) -> HashMap<i64, usize> {
    let mut counts = HashMap::new();
    for term in terms {
        let count = segments
            .iter()
            .filter(|segment| term_matches(&segment.source.text, term))
            .count();
        counts.insert(term.synthetic_id(), count);
    }
    counts
}

fn high_frequency_anchors<'a>(
    terms: &'a [GlossaryTerm],
    computed_counts: &HashMap<i64, usize>,
    limit: usize,
) -> Vec<&'a GlossaryTerm> {
    let mut anchors = terms
        .iter()
        .filter(|term| term.category.is_high_frequency_anchor())
        .map(|term| {
            let count = term
                .source_count
                .max(*computed_counts.get(&term.synthetic_id()).unwrap_or(&0));
            (term, count)
        })
        .filter(|(_, count)| *count > 0)
        .collect::<Vec<_>>();
    anchors.sort_by(|(a, ac), (b, bc)| {
        bc.cmp(ac)
            .then_with(|| {
                a.scope_kind
                    .priority()
                    .cmp(&b.scope_kind.priority())
                    .reverse()
            })
            .then_with(|| a.source_text.cmp(&b.source_text))
    });
    anchors
        .into_iter()
        .take(limit)
        .map(|(term, _)| term)
        .collect()
}

trait SyntheticId {
    fn synthetic_id(&self) -> i64;
}

impl SyntheticId for GlossaryTerm {
    fn synthetic_id(&self) -> i64 {
        self.id.unwrap_or_else(|| {
            let mut hash = 0xcbf29ce484222325_u64;
            for byte in format!(
                "{}\0{}\0{}\0{}",
                self.scope_kind.as_str(),
                self.scope_id.as_deref().unwrap_or(""),
                self.source_language,
                self.source_text
            )
            .as_bytes()
            {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(0x100000001b3);
            }
            i64::from_ne_bytes(hash.to_ne_bytes())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ir::{BlockId, SectionId},
        segment::{
            Segment, SegmentBlock, SegmentConstraints, SegmentContext, SegmentId, SegmentMetadata,
            SegmentSource,
        },
    };

    #[test]
    fn book_scope_overrides_series_scope() {
        let terms = vec![
            term("Aragorn", "Aragorn", GlossaryScopeKind::Series),
            term("Aragorn", "Granpasso", GlossaryScopeKind::Book),
        ];
        let merged = merge_scope_terms(&terms);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].target_text, "Granpasso");
    }

    #[test]
    fn merge_preserves_case_sensitive_source_variants() {
        let mut proper_name = term("Will", "Will", GlossaryScopeKind::Book);
        proper_name.case_sensitive = true;
        let mut auxiliary = term("will", "volonta", GlossaryScopeKind::Book);
        auxiliary.case_sensitive = true;

        let merged = merge_scope_terms(&[proper_name, auxiliary]);

        assert_eq!(merged.len(), 2);
        assert!(merged.iter().any(|term| term.source_text == "Will"));
        assert!(merged.iter().any(|term| term.source_text == "will"));
    }

    #[test]
    fn selects_matched_always_recent_and_high_frequency_terms() {
        let mut ring = term("Ring", "Anello", GlossaryScopeKind::Book);
        ring.category = GlossaryCategory::Object;
        ring.source_count = 100;
        let mut style = term("you", "tu", GlossaryScopeKind::Book);
        style.category = GlossaryCategory::Style;
        style.always_active = true;
        let terms = vec![ring, style];
        let segments = vec![
            segment("seg_1", 0, "The Ring is here"),
            segment("seg_2", 1, "He lifted it"),
        ];

        let selected = select_glossary_for_segments(&segments, &terms, 800);
        let second = &selected.entries_by_segment["seg_2"];
        assert!(second.iter().any(|entry| entry.source == "Ring"));
        assert!(second.iter().any(|entry| entry.source == "you"));
    }

    fn term(source: &str, target: &str, scope_kind: GlossaryScopeKind) -> GlossaryTerm {
        GlossaryTerm {
            id: None,
            scope_kind,
            scope_id: Some("scope".to_string()),
            source_text: source.to_string(),
            target_text: target.to_string(),
            category: GlossaryCategory::Person,
            notes: None,
            case_sensitive: false,
            always_active: false,
            status: GlossaryStatus::UserSeeded,
            source_language: "English".to_string(),
            target_language: "Italian".to_string(),
            source_count: 0,
        }
    }

    fn segment(id: &str, ordinal: usize, text: &str) -> Segment {
        let block_id = BlockId(format!("b_{ordinal:06}"));
        Segment {
            id: SegmentId(id.to_string()),
            section_id: SectionId("sec_1".to_string()),
            ordinal,
            block_ids: vec![block_id.clone()],
            source: SegmentSource {
                text: text.to_string(),
                blocks: vec![SegmentBlock {
                    block_id,
                    kind: "paragraph".to_string(),
                    text: text.to_string(),
                    text_runs: Vec::new(),
                    protected_spans: Vec::new(),
                }],
                token_estimate: text.len() / 4,
            },
            context: SegmentContext::default(),
            metadata: SegmentMetadata::default(),
            constraints: SegmentConstraints::default(),
            checksum: id.to_string(),
        }
    }
}
