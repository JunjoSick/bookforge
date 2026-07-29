use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::{
    ir::{Block, BlockKind},
    marker::parse_paired_marker_open,
    segment::Segment,
};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlossaryCandidate {
    pub source_text: String,
    pub category: GlossaryCategory,
    pub source_count: usize,
}

#[derive(Debug, Clone)]
struct CandidateStats {
    category: GlossaryCategory,
    source_count: usize,
    /// Occurrences that are capitalised for some reason other than position —
    /// mid-sentence, outside a heading. A candidate with none of these is a
    /// common word that only ever opened a sentence.
    evidence_count: usize,
    forms: BTreeMap<String, usize>,
}

pub fn extract_glossary_candidates(
    blocks: &[Block],
    source_language: &str,
    min_count: usize,
    limit: Option<usize>,
) -> Vec<GlossaryCandidate> {
    let stopwords = common_words(source_language);
    let mut candidates = BTreeMap::<String, CandidateStats>::new();

    for block in blocks {
        let quoted_italic_sources = collect_quoted_italic_candidates(block, &mut candidates);
        let visible_text = block_visible_text(block);
        collect_capitalized_candidates(
            &visible_text,
            stopwords,
            &quoted_italic_sources,
            matches!(block.kind, BlockKind::Heading(_)),
            &mut candidates,
        );
    }

    let min_count = min_count.max(1);
    let word_evidence = candidates
        .iter()
        .filter(|(key, _)| !key.contains(' '))
        .map(|(key, stats)| (key.clone(), stats.evidence_count))
        .collect::<HashMap<_, _>>();
    let mut candidates = candidates
        .into_iter()
        .filter(|(key, stats)| {
            // An author's own italics or quotes are an explicit signal, so
            // `Invented` bypasses both frequency and positional evidence.
            stats.category == GlossaryCategory::Invented
                || (stats.source_count >= min_count
                    && has_positional_evidence(key, stats, &word_evidence))
        })
        .map(|(_, stats)| stats)
        .map(|stats| GlossaryCandidate {
            source_text: preferred_form(&stats.forms),
            category: stats.category,
            source_count: stats.source_count,
        })
        .collect::<Vec<_>>();

    candidates.sort_by(|left, right| {
        right
            .source_count
            .cmp(&left.source_count)
            .then_with(|| left.source_text.cmp(&right.source_text))
            .then_with(|| left.category.as_str().cmp(right.category.as_str()))
    });
    if let Some(limit) = limit {
        candidates.truncate(limit);
    }
    candidates
}

/// Return one short source excerpt containing a glossary candidate.
///
/// Candidate rows intentionally stay small and source-document agnostic. The
/// caller can derive this disposable context from the EPUB when it is needed
/// for a proposal prompt.
pub fn glossary_candidate_excerpt(
    blocks: &[Block],
    source_text: &str,
    max_chars: usize,
) -> Option<String> {
    let source_text = source_text.trim();
    if source_text.is_empty() {
        return None;
    }

    blocks.iter().find_map(|block| {
        let text = block_visible_text(block);
        let start = text.find(source_text).or_else(|| {
            source_text
                .is_ascii()
                .then(|| {
                    text.to_ascii_lowercase()
                        .find(&source_text.to_ascii_lowercase())
                })
                .flatten()
        })?;
        let start_char = text[..start].chars().count();
        let term_chars = source_text.chars().count();
        Some(sentence_excerpt(
            &text,
            start_char,
            start_char + term_chars,
            max_chars,
        ))
    })
}

fn sentence_excerpt(text: &str, term_start: usize, term_end: usize, max_chars: usize) -> String {
    let chars = text.chars().collect::<Vec<_>>();
    let sentence_start = chars[..term_start]
        .iter()
        .rposition(|ch| matches!(ch, '.' | '!' | '?'))
        .map(|index| index + 1)
        .unwrap_or(0);
    let sentence_end = chars[term_end..]
        .iter()
        .position(|ch| matches!(ch, '.' | '!' | '?'))
        .map(|index| term_end + index + 1)
        .unwrap_or(chars.len());

    let max_chars = max_chars.max(term_end.saturating_sub(term_start)).max(1);
    let mut excerpt_start = sentence_start;
    let mut excerpt_end = sentence_end;
    if excerpt_end.saturating_sub(excerpt_start) > max_chars {
        let term_len = term_end.saturating_sub(term_start);
        let context = max_chars.saturating_sub(term_len);
        excerpt_start = term_start.saturating_sub(context / 2).max(sentence_start);
        excerpt_end =
            (term_end + context.saturating_sub(term_start - excerpt_start)).min(sentence_end);
        if excerpt_end.saturating_sub(excerpt_start) < max_chars {
            excerpt_start = excerpt_end.saturating_sub(max_chars).max(sentence_start);
        }
    }

    let mut excerpt = chars[excerpt_start..excerpt_end]
        .iter()
        .collect::<String>()
        .trim()
        .to_string();
    let mut excerpt_chars = excerpt.chars().count();
    if excerpt_start > sentence_start && excerpt_chars < max_chars {
        excerpt.insert(0, '…');
        excerpt_chars += 1;
    }
    if excerpt_end < sentence_end && excerpt_chars < max_chars {
        excerpt.push('…');
    }
    excerpt
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GlossarySelectionRule {
    SegmentMatched,
    AlwaysActive,
    RecentlyActive,
    HighFrequencyAnchor,
}

impl GlossarySelectionRule {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SegmentMatched => "segment_matched",
            Self::AlwaysActive => "always_active",
            Self::RecentlyActive => "recently_active",
            Self::HighFrequencyAnchor => "high_frequency_anchor",
        }
    }
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
    /// Selection rules aligned by index with `entries_by_segment`.
    ///
    /// This metadata is intentionally kept out of `GlossaryPromptTerm` so it
    /// never changes the serialized prompt or its cache identity.
    pub rules_by_segment: HashMap<String, Vec<GlossarySelectionRule>>,
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
    let mut rules_by_segment = HashMap::new();
    let mut truncated_authoritative_entries = 0usize;

    for (index, segment) in segments.iter().enumerate() {
        let mut selected = Vec::<(&GlossaryTerm, GlossarySelectionRule)>::new();
        let mut seen = HashSet::<i64>::new();

        for term in &terms {
            if term_matches(&segment.source.text, term) {
                push_term(
                    &mut selected,
                    &mut seen,
                    term,
                    GlossarySelectionRule::SegmentMatched,
                );
            }
        }

        for term in terms.iter().filter(|term| term.always_active) {
            push_term(
                &mut selected,
                &mut seen,
                term,
                GlossarySelectionRule::AlwaysActive,
            );
        }

        let start = index.saturating_sub(5);
        for previous in &segments[start..index] {
            if previous.section_id != segment.section_id {
                continue;
            }
            for term in &terms {
                if term_matches(&previous.source.text, term) {
                    push_term(
                        &mut selected,
                        &mut seen,
                        term,
                        GlossarySelectionRule::RecentlyActive,
                    );
                }
            }
        }

        for term in &high_frequency {
            push_term(
                &mut selected,
                &mut seen,
                term,
                GlossarySelectionRule::HighFrequencyAnchor,
            );
        }

        let (bounded, truncated) = enforce_budget(selected, budget_tokens);
        truncated_authoritative_entries += truncated;
        let (entries, rules) = bounded
            .into_iter()
            .map(|(term, rule)| (GlossaryPromptTerm::from_term(term), rule))
            .unzip();
        entries_by_segment.insert(segment.id.0.clone(), entries);
        rules_by_segment.insert(segment.id.0.clone(), rules);
    }

    SegmentGlossarySelections {
        entries_by_segment,
        rules_by_segment,
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
    selected: &mut Vec<(&'a GlossaryTerm, GlossarySelectionRule)>,
    seen: &mut HashSet<i64>,
    term: &'a GlossaryTerm,
    rule: GlossarySelectionRule,
) {
    let synthetic = term.synthetic_id();
    if seen.insert(synthetic) {
        selected.push((term, rule));
    }
}

fn enforce_budget(
    terms: Vec<(&GlossaryTerm, GlossarySelectionRule)>,
    budget_tokens: usize,
) -> (Vec<(&GlossaryTerm, GlossarySelectionRule)>, usize) {
    let mut used = 0usize;
    let mut kept = Vec::new();
    let mut truncated = 0usize;
    for (term, rule) in terms {
        let estimate = estimate_prompt_tokens(term);
        if used + estimate <= budget_tokens || kept.is_empty() {
            used += estimate;
            kept.push((term, rule));
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

/// Whether a candidate is capitalised for some reason other than where it sits.
///
/// A single word needs at least one mid-sentence, non-heading occurrence:
/// `Finally` and `Meanwhile` accumulate counts purely from sentence position
/// and are not terminology.
///
/// A multi-word phrase gets a second chance. `Ivan Ilych` may open every
/// sentence it appears in, but it is still a name if each of its words is
/// independently attested mid-sentence. That distinguishes it from
/// `Finally Klapaucius`, where the leading word is attested nowhere. A word
/// missing from the map was suppressed as an author-marked italic, which is
/// evidence in its own right.
fn has_positional_evidence(
    key: &str,
    stats: &CandidateStats,
    word_evidence: &HashMap<String, usize>,
) -> bool {
    if stats.evidence_count > 0 {
        return true;
    }
    key.contains(' ')
        && key
            .split(' ')
            .all(|word| word_evidence.get(word).copied().unwrap_or(1) > 0)
}

fn collect_capitalized_candidates(
    text: &str,
    stopwords: &[&str],
    skip_sources: &HashSet<String>,
    in_heading: bool,
    candidates: &mut BTreeMap<String, CandidateStats>,
) {
    let words = tokenize_words(text);
    let mut index = 0usize;
    while index < words.len() {
        let word = &words[index];
        if !is_capitalized_candidate_word(&word.text) {
            index += 1;
            continue;
        }

        // A heading is title-cased by convention, so nothing in it is
        // evidence; neither is a word that merely opens a sentence. Such
        // occurrences still count toward frequency — they just cannot be the
        // only reason a word became a candidate.
        let evidential = !in_heading && !word.sentence_initial;

        if !is_common_word(&word.text, stopwords)
            && !skip_sources.contains(&word.text.to_lowercase())
        {
            add_candidate(candidates, &word.text, GlossaryCategory::Other, evidential);
        }

        let start = index;
        let mut end = index;
        while end < words.len()
            && is_capitalized_candidate_word(&words[end].text)
            && !is_common_word(&words[end].text, stopwords)
        {
            end += 1;
        }
        if end.saturating_sub(start) >= 2 {
            let phrase = words[start..end]
                .iter()
                .map(|token| token.text.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            if !skip_sources.contains(&phrase.to_lowercase()) {
                // The phrase inherits its first word's position: every later
                // word in it is mid-sentence by construction.
                add_candidate(candidates, &phrase, GlossaryCategory::Other, evidential);
            }
        }
        index += 1;
    }
}

fn collect_quoted_italic_candidates(
    block: &Block,
    candidates: &mut BTreeMap<String, CandidateStats>,
) -> HashSet<String> {
    let italic_ids = block
        .inline_marks
        .iter()
        .filter(|mark| {
            let kind = mark.kind.to_ascii_lowercase();
            kind == "em" || kind == "i"
        })
        .map(|mark| mark.id.as_str())
        .collect::<HashSet<_>>();
    if italic_ids.is_empty() {
        return HashSet::new();
    }

    let mut sources = HashSet::new();
    let marked = marked_block_text(block);
    let mut offset = 0usize;
    while let Some(relative_start) = marked[offset..].find('<') {
        let tag_start = offset + relative_start;
        let tag = &marked[tag_start..];
        let Some(open) = parse_paired_marker_open(tag) else {
            offset = tag_start + 1;
            continue;
        };
        if !open.id.starts_with('m') {
            offset = tag_start + open.len;
            continue;
        }
        let tag_end = tag_start + open.len;
        let close = format!("</{}>", open.tag_name);
        let Some(relative_close) = marked[tag_end..].find(&close) else {
            break;
        };
        let close_start = tag_end + relative_close;
        let close_end = close_start + close.len();
        if italic_ids.contains(open.id.as_str()) {
            let raw_content = &marked[tag_end..close_start];
            if let Some(phrase) = quoted_italic_phrase(&marked, tag_start, close_end, raw_content) {
                sources.insert(phrase.to_lowercase());
                add_candidate(candidates, &phrase, GlossaryCategory::Invented, true);
            }
        }
        offset = close_end;
    }
    sources
}

fn quoted_italic_phrase(
    marked_text: &str,
    marker_start: usize,
    marker_end: usize,
    raw_content: &str,
) -> Option<String> {
    let content = normalize_candidate_text(&strip_marker_tokens(raw_content));
    if content.is_empty() {
        return None;
    }
    if let Some(inner) = trim_enclosing_quotes(&content) {
        return nonempty_candidate(inner);
    }

    let before = previous_visible_char(&marked_text[..marker_start]);
    let after = next_visible_char(&marked_text[marker_end..]);
    if before.zip(after).is_some_and(|(left, right)| {
        is_quote_pair(left, right) || (is_quote(left) && is_quote(right))
    }) {
        return Some(content);
    }
    None
}

fn add_candidate(
    candidates: &mut BTreeMap<String, CandidateStats>,
    source_text: &str,
    category: GlossaryCategory,
    evidential: bool,
) {
    let source_text = normalize_candidate_text(source_text);
    if source_text.chars().filter(|ch| ch.is_alphabetic()).count() < 2 {
        return;
    }
    let key = source_text.to_lowercase();
    let entry = candidates.entry(key).or_insert_with(|| CandidateStats {
        category,
        source_count: 0,
        evidence_count: 0,
        forms: BTreeMap::new(),
    });
    if category == GlossaryCategory::Invented {
        entry.category = GlossaryCategory::Invented;
    }
    entry.source_count += 1;
    if evidential {
        entry.evidence_count += 1;
    }
    *entry.forms.entry(source_text).or_insert(0) += 1;
}

fn preferred_form(forms: &BTreeMap<String, usize>) -> String {
    forms
        .iter()
        .max_by(|left, right| left.1.cmp(right.1).then_with(|| right.0.cmp(left.0)))
        .map(|(form, _)| form.clone())
        .unwrap_or_default()
}

/// A word plus whether it sits where English capitalises by convention.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WordToken {
    text: String,
    /// True at the start of a block, a sentence, a quotation, or a
    /// parenthetical. Capitalisation in those positions is grammar, not
    /// evidence that the word is a name.
    sentence_initial: bool,
}

/// Characters after which English capitalises the following word regardless of
/// what that word is. Sentence terminators and line breaks are the obvious
/// cases; quotation and parenthesis openers matter just as much, because
/// `"Good morning," he said` capitalises `Good` for exactly the same reason
/// `Finally, he left` capitalises `Finally`.
///
/// Deliberately excluded: `'` and `’`, which are overwhelmingly apostrophes in
/// prose; `:`, `;` and dashes, which do not trigger capitalisation in English.
/// Over-marking costs recall — a term marked sentence-initial everywhere is
/// dropped — so the set stays narrow.
fn opens_a_sentence(ch: char) -> bool {
    matches!(
        ch,
        '.' | '!' | '?' | '…' | '\n' | '\r' | '"' | '“' | '„' | '«' | '‘' | '‹' | '(' | '['
    )
}

// The two `current.push(ch)` branches look identical to clippy but guard
// semantically distinct cases (alphabetic char vs. internal-word connector
// with lookahead). Collapsing them with `||` would obscure intent.
#[allow(clippy::if_same_then_else)]
fn tokenize_words(text: &str) -> Vec<WordToken> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars().peekable();
    let mut sentence_initial = true;

    while let Some(ch) = chars.next() {
        if ch.is_alphabetic() {
            current.push(ch);
        } else if is_internal_word_connector(ch)
            && !current.is_empty()
            && chars.peek().is_some_and(|next| next.is_alphabetic())
        {
            current.push(ch);
        } else {
            if !current.is_empty() {
                words.push(WordToken {
                    text: std::mem::take(&mut current),
                    sentence_initial,
                });
                sentence_initial = false;
            }
            if opens_a_sentence(ch) {
                sentence_initial = true;
            }
        }
    }

    if !current.is_empty() {
        words.push(WordToken {
            text: current,
            sentence_initial,
        });
    }
    words
}

fn is_capitalized_candidate_word(word: &str) -> bool {
    let mut alphabetic = word.chars().filter(|ch| ch.is_alphabetic());
    let Some(first) = alphabetic.next() else {
        return false;
    };
    first.is_uppercase()
        && word.chars().filter(|ch| ch.is_alphabetic()).count() > 1
        && word.chars().any(|ch| ch.is_lowercase())
}

fn is_common_word(word: &str, stopwords: &[&str]) -> bool {
    let key = word.to_lowercase();
    if stopwords.contains(&key.as_str()) {
        return true;
    }
    // A contraction is as common as the pronoun it is built from. `I'll` and
    // `It's` sit mid-sentence often enough to pass the positional test, so the
    // stem is what disqualifies them — while `Trurl's` keeps its stem `trurl`
    // and stays a candidate.
    key.split_once(['\'', '’'])
        .is_some_and(|(stem, _)| stopwords.contains(&stem))
}

fn is_internal_word_connector(ch: char) -> bool {
    matches!(ch, '\'' | '’' | '-' | '‐' | '‑')
}

fn marked_block_text(block: &Block) -> String {
    block
        .text_runs
        .iter()
        .map(|run| run.text.as_str())
        .collect::<Vec<_>>()
        .join("")
}

fn block_visible_text(block: &Block) -> String {
    normalize_candidate_text(&strip_marker_tokens(&marked_block_text(block)))
}

fn strip_marker_tokens(text: &str) -> String {
    crate::marker::strip_marker_tokens(text)
}

fn previous_visible_char(text: &str) -> Option<char> {
    strip_marker_tokens(text)
        .chars()
        .rev()
        .find(|ch| !ch.is_whitespace())
}

fn next_visible_char(text: &str) -> Option<char> {
    strip_marker_tokens(text)
        .chars()
        .find(|ch| !ch.is_whitespace())
}

fn normalize_candidate_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn trim_enclosing_quotes(text: &str) -> Option<&str> {
    let mut chars = text.char_indices();
    let (_, first) = chars.next()?;
    let (last_start, last) = text.char_indices().next_back()?;
    if first.len_utf8() >= text.len() || !is_quote_pair(first, last) {
        return None;
    }
    Some(text[first.len_utf8()..last_start].trim())
}

fn nonempty_candidate(text: &str) -> Option<String> {
    let normalized = normalize_candidate_text(text);
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn is_quote_pair(left: char, right: char) -> bool {
    matches!(
        (left, right),
        ('"', '"') | ('\'', '\'') | ('“', '”') | ('‘', '’') | ('«', '»') | ('„', '“')
    )
}

fn is_quote(ch: char) -> bool {
    matches!(ch, '"' | '\'' | '“' | '”' | '‘' | '’' | '«' | '»' | '„')
}

fn common_words(source_language: &str) -> &'static [&'static str] {
    let normalized = source_language.to_lowercase();
    if normalized == "en" || normalized.starts_with("en-") || normalized.contains("english") {
        ENGLISH_COMMON_WORDS
    } else {
        FALLBACK_COMMON_WORDS
    }
}

const FALLBACK_COMMON_WORDS: &[&str] = &[
    "a", "an", "and", "as", "at", "but", "by", "for", "from", "in", "into", "of", "on", "or",
    "the", "to", "with",
];

const ENGLISH_COMMON_WORDS: &[&str] = &[
    "a", "about", "after", "again", "all", "also", "an", "and", "another", "any", "are", "as",
    "at", "away", "be", "because", "been", "before", "being", "but", "by", "came", "can", "come",
    "could", "day", "did", "do", "does", "down", "each", "even", "every", "for", "from", "get",
    "go", "had", "has", "have", "he", "her", "here", "him", "his", "how", "i", "if", "in", "into",
    "is", "it", "its", "just", "like", "made", "make", "man", "many", "me", "more", "much", "must",
    "my", "no", "not", "now", "of", "off", "on", "one", "only", "or", "other", "our", "out",
    "over", "said", "same", "see", "she", "should", "so", "some", "such", "than", "that", "the",
    "their", "them", "then", "there", "these", "they", "this", "those", "through", "time", "to",
    "too", "up", "very", "was", "way", "we", "well", "were", "what", "when", "where", "which",
    "while", "who", "will", "with", "would", "you", "your",
];

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
        ir::{Block, BlockId, BlockKind, DomPath, InlineMark, SectionId, TextRun},
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

    #[test]
    fn selection_rules_are_credited_only_when_they_inject_an_entry() {
        let mut matched = term("Matched", "Corrisposto", GlossaryScopeKind::Book);
        matched.category = GlossaryCategory::Phrase;
        let mut always = term("Always", "Sempre", GlossaryScopeKind::Book);
        always.category = GlossaryCategory::Style;
        always.always_active = true;
        let mut recent = term("Recent", "Recente", GlossaryScopeKind::Book);
        recent.category = GlossaryCategory::Phrase;
        let mut anchor = term("Anchor", "Ancora", GlossaryScopeKind::Book);
        anchor.category = GlossaryCategory::Person;
        anchor.source_count = 100;
        let segments = vec![
            segment("seg_1", 0, "Recent appeared earlier"),
            segment("seg_2", 1, "Matched appears now"),
        ];

        let selections =
            select_glossary_for_segments(&segments, &[matched, always, recent, anchor], 800);
        let entries = &selections.entries_by_segment["seg_2"];
        let rules = &selections.rules_by_segment["seg_2"];
        let credited = entries
            .iter()
            .zip(rules)
            .map(|(entry, rule)| (entry.source.as_str(), *rule))
            .collect::<HashMap<_, _>>();

        assert_eq!(credited["Matched"], GlossarySelectionRule::SegmentMatched);
        assert_eq!(credited["Always"], GlossarySelectionRule::AlwaysActive);
        assert_eq!(credited["Recent"], GlossarySelectionRule::RecentlyActive);
        assert_eq!(
            credited["Anchor"],
            GlossarySelectionRule::HighFrequencyAnchor
        );
        assert_eq!(credited.len(), 4);
    }

    #[test]
    fn extracts_repeated_capitalized_names_and_counts() {
        let blocks = vec![block(
            "Ivan Ilych met Peter Ivanovich. Ivan Ilych greeted Ivan again.",
        )];

        let candidates = extract_glossary_candidates(&blocks, "English", 2, None);

        assert!(
            candidates.iter().any(|candidate| {
                candidate.source_text == "Ivan" && candidate.source_count == 3
            }),
            "{candidates:?}"
        );
        assert!(
            candidates.iter().any(|candidate| {
                candidate.source_text == "Ivan Ilych" && candidate.source_count == 2
            }),
            "{candidates:?}"
        );
    }

    #[test]
    fn extraction_filters_common_sentence_words() {
        let blocks = vec![block(
            "The Court waited. The Court spoke. Then Court adjourned.",
        )];

        let candidates = extract_glossary_candidates(&blocks, "English", 2, None);

        assert!(
            !candidates
                .iter()
                .any(|candidate| candidate.source_text == "The")
        );
        assert!(
            !candidates
                .iter()
                .any(|candidate| candidate.source_text == "Then")
        );
        assert!(
            candidates.iter().any(|candidate| {
                candidate.source_text == "Court" && candidate.source_count == 3
            })
        );
    }

    #[test]
    fn extraction_ignores_words_capitalized_only_at_sentence_start() {
        let blocks = vec![block(
            "Finally the door opened. Finally he spoke. Finally it ended. \
             Finally they left.",
        )];

        let candidates = extract_glossary_candidates(&blocks, "English", 2, None);

        assert!(
            !candidates
                .iter()
                .any(|candidate| candidate.source_text == "Finally"),
            "{candidates:?}"
        );
    }

    #[test]
    fn extraction_keeps_words_capitalized_mid_sentence() {
        let blocks = vec![block(
            "Finally the door opened. Finally he spoke. Then Finally arrived, \
             and Finally departed.",
        )];

        let candidates = extract_glossary_candidates(&blocks, "English", 2, None);

        let finally = candidates
            .iter()
            .find(|candidate| candidate.source_text == "Finally")
            .unwrap_or_else(|| panic!("{candidates:?}"));
        // Every occurrence still counts toward frequency; position only
        // decides whether the word qualifies at all.
        assert_eq!(finally.source_count, 4);
    }

    #[test]
    fn extraction_treats_a_quotation_opener_as_sentence_start() {
        let blocks = vec![block(
            "He said, “Good day.” She said, “Good day.” They said, “Good day.”",
        )];

        let candidates = extract_glossary_candidates(&blocks, "English", 2, None);

        assert!(
            !candidates
                .iter()
                .any(|candidate| candidate.source_text == "Good"),
            "{candidates:?}"
        );
    }

    #[test]
    fn extraction_ignores_title_case_headings_as_evidence() {
        let blocks = vec![
            heading_block("The Third Sally"),
            heading_block("The Third Sally"),
            heading_block("The Third Sally"),
        ];

        let candidates = extract_glossary_candidates(&blocks, "English", 2, None);

        // A heading capitalises `Third` by typographic convention, so headings
        // alone can never promote a word to a candidate.
        assert!(candidates.is_empty(), "{candidates:?}");
    }

    #[test]
    fn extraction_keeps_heading_terms_attested_in_prose() {
        let blocks = vec![
            heading_block("The Third Sally"),
            block("They spoke of the Sally often, and the Sally was long."),
        ];

        let candidates = extract_glossary_candidates(&blocks, "English", 2, None);

        let sally = candidates
            .iter()
            .find(|candidate| candidate.source_text == "Sally")
            .unwrap_or_else(|| panic!("{candidates:?}"));
        assert_eq!(sally.source_count, 3);
    }

    #[test]
    fn extraction_keeps_a_name_phrase_that_always_opens_a_sentence() {
        // `Ivan Ilych` opens every sentence it appears in, but both of its
        // words are attested mid-sentence, which is what separates it from
        // `Finally Klapaucius`.
        let blocks = vec![block(
            "Ivan Ilych met Peter Ivanovich. Ivan Ilych greeted Ivan again.",
        )];

        let candidates = extract_glossary_candidates(&blocks, "English", 2, None);

        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.source_text == "Ivan Ilych"),
            "{candidates:?}"
        );
    }

    #[test]
    fn extraction_drops_a_phrase_led_by_an_unattested_word() {
        let blocks = vec![block(
            "Finally Klapaucius arrived. Finally Klapaucius spoke. \
             Trurl watched Klapaucius closely.",
        )];

        let candidates = extract_glossary_candidates(&blocks, "English", 2, None);

        assert!(
            !candidates
                .iter()
                .any(|candidate| candidate.source_text == "Finally Klapaucius"),
            "{candidates:?}"
        );
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.source_text == "Klapaucius"),
            "{candidates:?}"
        );
    }

    #[test]
    fn extraction_drops_contractions_of_common_words() {
        let blocks = vec![block(
            "He said I'll go and I'll stay. She said It's fine and It's over. \
             Trurl's plan worked, and Trurl's friend agreed.",
        )];

        let candidates = extract_glossary_candidates(&blocks, "English", 2, None);

        for junk in ["I'll", "It's"] {
            assert!(
                !candidates
                    .iter()
                    .any(|candidate| candidate.source_text == junk),
                "{junk} survived: {candidates:?}"
            );
        }
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.source_text == "Trurl's"),
            "{candidates:?}"
        );
    }

    #[test]
    fn extraction_discovers_quoted_italic_invented_phrases() {
        let blocks = vec![marked_block(
            vec!["He whispered “", "<m1>Lukh</m1>", "” once."],
            vec![InlineMark {
                id: "m1".to_string(),
                kind: "em".to_string(),
            }],
        )];

        let candidates = extract_glossary_candidates(&blocks, "English", 4, None);

        assert!(
            candidates.iter().any(|candidate| {
                candidate.source_text == "Lukh"
                    && candidate.category == GlossaryCategory::Invented
                    && candidate.source_count == 1
            }),
            "{candidates:?}"
        );
    }

    #[test]
    fn extraction_deduplicates_case_variants_with_preferred_count() {
        let blocks = vec![block(
            "Gerasim helped Gerasim. GERASIM shouted. Gerasim helped.",
        )];

        let candidates = extract_glossary_candidates(&blocks, "English", 2, None);
        let gerasim = candidates
            .iter()
            .find(|candidate| candidate.source_text == "Gerasim")
            .expect("Gerasim should be extracted");

        assert_eq!(gerasim.source_count, 3);
        assert_eq!(
            candidates
                .iter()
                .filter(|candidate| candidate.source_text.eq_ignore_ascii_case("gerasim"))
                .count(),
            1
        );
    }

    #[test]
    fn candidate_excerpt_selects_and_bounds_a_sentence_with_the_term() {
        let blocks = vec![block(
            "An unrelated opening. Trurl inspected the phantasmatron before dawn. A final line.",
        )];

        let excerpt =
            glossary_candidate_excerpt(&blocks, "phantasmatron", 42).expect("excerpt should exist");

        assert!(excerpt.contains("phantasmatron"), "{excerpt}");
        assert!(excerpt.chars().count() <= 42, "{excerpt}");
        assert!(!excerpt.contains("unrelated"), "{excerpt}");
        assert!(!excerpt.contains("final line"), "{excerpt}");
    }

    #[test]
    fn candidate_excerpt_is_case_insensitive_for_ascii_terms() {
        let blocks = vec![block("The STEELYPIPS arrived at noon.")];

        let excerpt =
            glossary_candidate_excerpt(&blocks, "Steelypips", 80).expect("excerpt should exist");

        assert_eq!(excerpt, "The STEELYPIPS arrived at noon.");
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

    fn block(text: &str) -> Block {
        marked_block(vec![text], Vec::new())
    }

    fn heading_block(text: &str) -> Block {
        let mut block = marked_block(vec![text], Vec::new());
        block.kind = BlockKind::Heading(1);
        block
    }

    fn marked_block(text_runs: Vec<&str>, inline_marks: Vec<InlineMark>) -> Block {
        Block {
            id: BlockId("b_000000".to_string()),
            section_id: SectionId("sec_1".to_string()),
            kind: BlockKind::Paragraph,
            dom_path: DomPath(vec![0]),
            text_runs: text_runs
                .into_iter()
                .enumerate()
                .map(|(index, text)| TextRun {
                    id: format!("r000000_{index:03}"),
                    text: text.to_string(),
                })
                .collect(),
            inline_marks,
            protected_spans: Vec::new(),
            token_estimate: 1,
        }
    }
}
