use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    BookforgeError, Result,
    config::{BilingualMode, BilingualStyle, ContextScope, JsonMode, SegmentationConfig},
    ir::{Block, BlockId, BlockKind, Book, ProtectedSpan, Section, SectionId},
};

/// Bumped when the cache key derivation changes incompatibly.
/// v3: token estimation switched from dominant-case-class weighting
/// (4.5 chars/token vs 1 char/token) to proportional per-character
/// script weights (`crate::token_estimate`). Segment groupings and
/// persisted block token estimates differ on mixed- and CJK-heavy
/// books, so cached rows from the old estimator are ineligible.
///
/// This legacy namespace is deliberately frozen at v3 for compatibility
/// with the CLI's resume path (which compares a recomputed namespace
/// against the persisted snapshot). The single structured cache identity
/// used by `bookforge_store` is [`CacheIdentity`] and its own
/// `CACHE_IDENTITY_SCHEMA_VERSION`; the two are unrelated.
pub const CACHE_KEY_SCHEMA_VERSION: u32 = 3;

/// Version of the structured cache identity serialization
/// ([`CacheIdentity::fingerprint`]). Bump once whenever any field or its
/// meaning changes incompatibly — every stored fingerprint changes and
/// previously cached rows (which carry an older fingerprint) can no longer
/// match, so ambiguous old entries are excluded forever without a data
/// migration.
///
/// v2: the identity now hashes the ACTUAL rendered prompt ingredients — the
/// ordered neighbor/context strings around each segment and the actual
/// rendered glossary/style/entity blocks — not just the configuration that
/// produced them. Two identical source texts in different contexts, or with
/// different per-segment glossary selections, can never reuse a cache row.
pub const CACHE_IDENTITY_SCHEMA_VERSION: u32 = 2;
/// Bumped when Segment / SegmentBlock layout changes incompatibly.
pub const SEGMENT_SCHEMA_VERSION: u32 = 1;
/// Stable label for the canonical unit checkpointed, retried, resumed, and
/// persisted in the job store.
pub const SEGMENT_UNIT_NAME: &str = "scheduler_segment";
/// Bumped when inline marker extraction (m/keep/ref) changes incompatibly.
/// v2: depth-anchored block closing, lazily anchored text blocks for
/// non-whitelist elements, addressable stray text nodes — block ordinals
/// and marker assignments differ from v1 on affected books.
/// v3: short per-block inline marker tags (`<m1>...</m1>`, `<r1/>`)
/// replace verbose global ids (`<m id="m000000_000">...</m>`).
/// v4: whitespace-only text nodes between adjacent inline elements stay
/// between marker tokens instead of moving inside the preceding marker.
pub const INLINE_MARKER_SCHEMA_VERSION: u32 = 4;

/// Historical path for the canonical script-aware token estimator.
pub use crate::token_estimate::estimate_tokens;

/// Compute a cache namespace that scopes lookups to a single set of
/// schema and segmentation parameters. Cached rows from a different
/// namespace are not eligible for reuse.
///
/// `glossary_fingerprint`, `style_fingerprint`, and `entities_fingerprint`
/// are opt-in mixins: pass an empty string to preserve cache compatibility
/// with runs that didn't use the feature; pass a non-empty fingerprint when
/// the rendered prompt actually changes. The three slots use distinct domain
/// separators so a fingerprint of one kind can never collide with another
/// kind's fingerprint of the same content (CORE-13).
#[allow(clippy::too_many_arguments)]
pub fn compute_cache_namespace(
    max_segment_tokens: usize,
    context_tokens: usize,
    profile: &str,
    batch_enabled: bool,
    prompt_version: &str,
    glossary_fingerprint: &str,
    style_fingerprint: &str,
    entities_fingerprint: &str,
) -> String {
    compute_cache_namespace_inner(
        CACHE_KEY_SCHEMA_VERSION,
        max_segment_tokens,
        context_tokens,
        profile,
        batch_enabled,
        prompt_version,
        Some(glossary_fingerprint),
        if style_fingerprint.is_empty() {
            None
        } else {
            Some(style_fingerprint)
        },
        if entities_fingerprint.is_empty() {
            None
        } else {
            Some(entities_fingerprint)
        },
    )
}

pub fn compute_cache_namespace_v1(
    max_segment_tokens: usize,
    context_tokens: usize,
    profile: &str,
    batch_enabled: bool,
    prompt_version: &str,
) -> String {
    compute_cache_namespace_inner(
        1,
        max_segment_tokens,
        context_tokens,
        profile,
        batch_enabled,
        prompt_version,
        None,
        None,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn compute_cache_namespace_inner(
    cache_key_schema_version: u32,
    max_segment_tokens: usize,
    context_tokens: usize,
    profile: &str,
    batch_enabled: bool,
    prompt_version: &str,
    glossary_fingerprint: Option<&str>,
    style_fingerprint: Option<&str>,
    entities_fingerprint: Option<&str>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(cache_key_schema_version.to_le_bytes());
    hasher.update(SEGMENT_SCHEMA_VERSION.to_le_bytes());
    hasher.update(INLINE_MARKER_SCHEMA_VERSION.to_le_bytes());
    hasher.update((max_segment_tokens as u64).to_le_bytes());
    hasher.update((context_tokens as u64).to_le_bytes());
    hasher.update(profile.as_bytes());
    hasher.update([batch_enabled as u8]);
    hasher.update(prompt_version.as_bytes());
    if let Some(glossary_fingerprint) =
        glossary_fingerprint.filter(|fingerprint| !fingerprint.is_empty())
    {
        hasher.update(b"|glossary|");
        hasher.update(glossary_fingerprint.as_bytes());
    }
    if let Some(style_fingerprint) = style_fingerprint {
        hasher.update(b"|style|");
        hasher.update(style_fingerprint.as_bytes());
    }
    if let Some(entities_fingerprint) = entities_fingerprint {
        hasher.update(b"|entities|");
        hasher.update(entities_fingerprint.as_bytes());
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// The single structured cache identity for a translation checkpoint.
///
/// Unlike the legacy [`compute_cache_namespace`] string (a flat bundle of a
/// handful of segmentation knobs), this struct captures *every* output-
/// affecting input — schema version, source identity, the effective
/// provider/model that actually produced the output, source/target language,
/// prompt template version and extra text, segmentation and context
/// window/budget/scope, the strict-context completion fence, batch shape,
/// compact-prompt mode, the style/glossary/entity fingerprints, bilingual
/// rendering, and the provider runtime settings that shape the request.
///
/// [`CacheIdentity::fingerprint`] serializes the fields deterministically
/// (fixed field order with domain-separated names, so no two distinct
/// inputs can collide) and is what `bookforge_store` persists per segment as
/// `segments.cache_fingerprint` and matches on during cache lookup. Rows
/// stamped before this identity existed carry an empty fingerprint and are
/// permanently ineligible for reuse.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CacheIdentity {
    /// [`CACHE_IDENTITY_SCHEMA_VERSION`] at the time the identity was
    /// serialized. A bump invalidates every stored fingerprint.
    pub schema_version: u32,
    /// Source content identity (`Segment::checksum`).
    pub source_hash: String,
    /// Effective provider that produced the output (not necessarily the
    /// primary run config — fallback outputs keep their real provenance).
    pub provider: String,
    /// Effective model that produced the output.
    pub model: String,
    pub source_lang: Option<String>,
    pub target_lang: String,
    /// Prompt template/schema version tag (`PromptVersion::as_str`).
    pub prompt_version: String,
    /// The legacy namespace bundle (segmentation/batch/profile/prompt
    /// version/fingerprints). Kept as a member so rows stamped before the
    /// structured identity still agree with their namespace-based lookup.
    pub cache_namespace: String,
    pub prompt_extra: Option<String>,
    pub max_segment_tokens: usize,
    pub context_tokens: usize,
    pub context_window: usize,
    pub context_budget_tokens: usize,
    pub context_scope: ContextScope,
    /// `Some(true)`/`Some(false)` when the run recorded its strict-context
    /// choice; `None` for legacy runs that never persisted it. `None` hashes
    /// to its own domain value so it can never match an explicit choice.
    pub strict_context: Option<bool>,
    pub profile: crate::config::TranslationProfile,
    pub batch_enabled: bool,
    pub batch_target_tokens: usize,
    pub batch_max_items: usize,
    pub batch_adaptive_sizing: bool,
    pub batch_split_on_json_failure: bool,
    pub batch_repair_invalid_items: bool,
    pub compact_prompts: bool,
    pub glossary_fingerprint: String,
    pub style_fingerprint: String,
    pub entities_fingerprint: String,
    /// Actual ordered neighbor content rendered immediately BEFORE this
    /// segment in the prompt (`Segment::context.before`, raw). Empty when the
    /// run has no context. Different neighbors at the same source text must
    /// never reuse a cache row, so this is hashed into the identity.
    pub context_before: String,
    /// Actual ordered neighbor content rendered immediately AFTER this
    /// segment in the prompt (`Segment::context.after`, raw).
    pub context_after: String,
    /// Canonical rendering of the actual per-segment glossary terms selected
    /// for this segment (ordered, budget-bounded). Empty when the run has no
    /// glossary. Two segments that select different terms are different
    /// prompts even if the config-level glossary fingerprint is identical.
    pub glossary_rendered: String,
    /// The actual rendered style-guide block substituted into the prompt
    /// (not just its config fingerprint).
    pub style_rendered: String,
    /// The actual rendered entity-agreement block substituted into the
    /// prompt (not just its config fingerprint).
    pub entities_rendered: String,
    pub bilingual_mode: BilingualMode,
    pub bilingual_separator: String,
    pub bilingual_style: BilingualStyle,
    pub thinking_disabled: bool,
    pub max_output_tokens: Option<u32>,
    pub batch_max_output_tokens: Option<u32>,
    pub json_mode: JsonMode,
}

/// Request inputs for [`CacheIdentity::minimal`]: the request-visible fields
/// (source identity, effective provider/model, languages, prompt tag, legacy
/// namespace). Every snapshot-only setting falls back to a constant
/// conservative default inside the identity.
#[derive(Debug, Clone, Copy)]
pub struct MinimalCacheIdentity<'a> {
    pub segment: &'a Segment,
    pub provider: &'a str,
    pub model: &'a str,
    pub source_lang: Option<&'a str>,
    pub target_lang: &'a str,
    pub prompt_version: &'a str,
    pub cache_namespace: &'a str,
}

impl CacheIdentity {
    /// Minimal identity for runs that have no persisted configuration
    /// snapshot (legacy databases and fixtures). The request-visible fields
    /// are real; every snapshot-only setting falls back to a constant
    /// conservative default, so identities never diverge for the same
    /// request inputs and always diverge when the request inputs differ.
    pub fn minimal(args: MinimalCacheIdentity<'_>) -> Self {
        Self {
            schema_version: CACHE_IDENTITY_SCHEMA_VERSION,
            source_hash: args.segment.checksum.clone(),
            provider: args.provider.to_string(),
            model: args.model.to_string(),
            source_lang: args.source_lang.map(str::to_string),
            target_lang: args.target_lang.to_string(),
            prompt_version: args.prompt_version.to_string(),
            cache_namespace: args.cache_namespace.to_string(),
            prompt_extra: None,
            max_segment_tokens: 0,
            context_tokens: 0,
            context_window: 0,
            context_budget_tokens: 0,
            context_scope: ContextScope::Chapter,
            strict_context: None,
            profile: crate::config::TranslationProfile::Balanced,
            batch_enabled: false,
            batch_target_tokens: 0,
            batch_max_items: 0,
            batch_adaptive_sizing: false,
            batch_split_on_json_failure: false,
            batch_repair_invalid_items: false,
            compact_prompts: false,
            glossary_fingerprint: String::new(),
            style_fingerprint: String::new(),
            entities_fingerprint: String::new(),
            context_before: args.segment.context.before.clone().unwrap_or_default(),
            context_after: args.segment.context.after.clone().unwrap_or_default(),
            glossary_rendered: String::new(),
            style_rendered: String::new(),
            entities_rendered: String::new(),
            bilingual_mode: BilingualMode::Replace,
            bilingual_separator: String::new(),
            bilingual_style: BilingualStyle::Minimal,
            thinking_disabled: false,
            max_output_tokens: None,
            batch_max_output_tokens: None,
            json_mode: JsonMode::Auto,
        }
    }

    /// Deterministic, domain-separated serialization of every identity
    /// field. Field names act as domain separators (a value cannot alias
    /// another field's name), and the schema version is hashed first so a
    /// bump changes every fingerprint.
    pub fn fingerprint(&self) -> String {
        let mut hasher = Sha256::new();
        feed(
            &mut hasher,
            "schema_version",
            &self.schema_version.to_le_bytes(),
        );
        feed(&mut hasher, "source_hash", self.source_hash.as_bytes());
        feed(&mut hasher, "provider", self.provider.as_bytes());
        feed(&mut hasher, "model", self.model.as_bytes());
        feed(
            &mut hasher,
            "source_lang",
            self.source_lang.as_deref().unwrap_or("").as_bytes(),
        );
        feed(&mut hasher, "target_lang", self.target_lang.as_bytes());
        feed(
            &mut hasher,
            "prompt_version",
            self.prompt_version.as_bytes(),
        );
        feed(
            &mut hasher,
            "cache_namespace",
            self.cache_namespace.as_bytes(),
        );
        feed(
            &mut hasher,
            "prompt_extra",
            self.prompt_extra.as_deref().unwrap_or("").as_bytes(),
        );
        feed(
            &mut hasher,
            "max_segment_tokens",
            &(self.max_segment_tokens as u64).to_le_bytes(),
        );
        feed(
            &mut hasher,
            "context_tokens",
            &(self.context_tokens as u64).to_le_bytes(),
        );
        feed(
            &mut hasher,
            "context_window",
            &(self.context_window as u64).to_le_bytes(),
        );
        feed(
            &mut hasher,
            "context_budget_tokens",
            &(self.context_budget_tokens as u64).to_le_bytes(),
        );
        feed(
            &mut hasher,
            "context_scope",
            self.context_scope.as_str().as_bytes(),
        );
        feed(
            &mut hasher,
            "strict_context",
            match self.strict_context {
                Some(true) => b"strict".as_slice(),
                Some(false) => b"loose".as_slice(),
                None => b"unknown".as_slice(),
            },
        );
        feed(
            &mut hasher,
            "profile",
            self.profile.namespace_str().as_bytes(),
        );
        feed(&mut hasher, "batch_enabled", &[self.batch_enabled as u8]);
        feed(
            &mut hasher,
            "batch_target_tokens",
            &(self.batch_target_tokens as u64).to_le_bytes(),
        );
        feed(
            &mut hasher,
            "batch_max_items",
            &(self.batch_max_items as u64).to_le_bytes(),
        );
        feed(
            &mut hasher,
            "batch_adaptive_sizing",
            &[self.batch_adaptive_sizing as u8],
        );
        feed(
            &mut hasher,
            "batch_split_on_json_failure",
            &[self.batch_split_on_json_failure as u8],
        );
        feed(
            &mut hasher,
            "batch_repair_invalid_items",
            &[self.batch_repair_invalid_items as u8],
        );
        feed(
            &mut hasher,
            "compact_prompts",
            &[self.compact_prompts as u8],
        );
        feed(
            &mut hasher,
            "glossary_fingerprint",
            self.glossary_fingerprint.as_bytes(),
        );
        feed(
            &mut hasher,
            "style_fingerprint",
            self.style_fingerprint.as_bytes(),
        );
        feed(
            &mut hasher,
            "entities_fingerprint",
            self.entities_fingerprint.as_bytes(),
        );
        feed(
            &mut hasher,
            "context_before",
            self.context_before.as_bytes(),
        );
        feed(&mut hasher, "context_after", self.context_after.as_bytes());
        feed(
            &mut hasher,
            "glossary_rendered",
            self.glossary_rendered.as_bytes(),
        );
        feed(
            &mut hasher,
            "style_rendered",
            self.style_rendered.as_bytes(),
        );
        feed(
            &mut hasher,
            "entities_rendered",
            self.entities_rendered.as_bytes(),
        );
        feed(
            &mut hasher,
            "bilingual_mode",
            self.bilingual_mode.as_str().as_bytes(),
        );
        feed(
            &mut hasher,
            "bilingual_separator",
            self.bilingual_separator.as_bytes(),
        );
        feed(
            &mut hasher,
            "bilingual_style",
            self.bilingual_style.as_str().as_bytes(),
        );
        feed(
            &mut hasher,
            "thinking_disabled",
            &[self.thinking_disabled as u8],
        );
        feed(
            &mut hasher,
            "max_output_tokens",
            &self.max_output_tokens.unwrap_or_default().to_le_bytes(),
        );
        feed(
            &mut hasher,
            "batch_max_output_tokens",
            &self
                .batch_max_output_tokens
                .unwrap_or_default()
                .to_le_bytes(),
        );
        feed(
            &mut hasher,
            "json_mode",
            match self.json_mode {
                JsonMode::Auto => b"auto".as_slice(),
                JsonMode::ResponseFormat => b"response_format".as_slice(),
                JsonMode::PromptOnly => b"prompt_only".as_slice(),
            },
        );
        let digest = hasher.finalize();
        let mut hex = String::with_capacity(digest.len() * 2);
        for byte in digest {
            use std::fmt::Write as _;
            write!(&mut hex, "{byte:02x}").expect("writing to string should not fail");
        }
        hex
    }
}

/// Feed one domain-separated field into the identity hasher. `0xff` opens a
/// field boundary (never appears in ASCII field names), the name ends at a
/// `0x00` terminator, and the raw value follows — so a value cannot be
/// confused with a later name.
fn feed(hasher: &mut Sha256, name: &str, value: &[u8]) {
    hasher.update([0xff]);
    hasher.update(name.as_bytes());
    hasher.update([0x00]);
    hasher.update(value);
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockTranslation {
    pub block_id: BlockId,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SegmentId(pub String);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Segment {
    pub id: SegmentId,
    pub section_id: SectionId,
    pub ordinal: usize,
    pub block_ids: Vec<BlockId>,
    pub source: SegmentSource,
    pub context: SegmentContext,
    pub metadata: SegmentMetadata,
    pub constraints: SegmentConstraints,
    pub checksum: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentSource {
    pub text: String,
    pub blocks: Vec<SegmentBlock>,
    pub token_estimate: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentBlock {
    pub block_id: BlockId,
    pub kind: String,
    pub text: String,
    pub text_runs: Vec<SegmentTextRun>,
    pub protected_spans: Vec<ProtectedSpan>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentTextRun {
    pub id: String,
    pub text: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SegmentContext {
    pub before: Option<String>,
    pub after: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SegmentMetadata {
    pub book_title: Option<String>,
    pub section_title: Option<String>,
    pub section_index: usize,
    pub segment_index_in_section: usize,
    pub total_segments_in_section: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SegmentConstraints {
    pub preserve_markers: Vec<String>,
    pub preserve_spans: Vec<String>,
    pub max_tokens: usize,
}

pub fn block_kind_label(kind: BlockKind) -> &'static str {
    match kind {
        BlockKind::Heading(_) => "heading",
        BlockKind::Paragraph => "paragraph",
        BlockKind::ListItem => "list_item",
        BlockKind::Quote => "quote",
        BlockKind::TableCell => "table_cell",
        BlockKind::TableRow => "table_row",
        BlockKind::Footnote => "footnote",
        BlockKind::Caption => "caption",
        BlockKind::Code => "code",
        BlockKind::PageFurniture => "page_furniture",
        BlockKind::Unknown => "unknown",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SegmentStatus {
    Queued,
    Succeeded,
    Failed,
    RetryPending,
    NeedsReview,
    SkippedCached,
}

/// Build the scheduler's units of work.
///
/// Each returned segment becomes one job-store `segments` row. Provider request
/// batching may group several of these units, but does not change their identity
/// or count.
pub fn build_segments(book: &Book, config: &SegmentationConfig) -> Result<Vec<Segment>> {
    if config.max_segment_tokens == 0 {
        return Err(BookforgeError::InvalidInput(
            "max_segment_tokens must be greater than zero".to_string(),
        ));
    }

    let mut segments = Vec::new();

    for (section_index, section) in book.sections.iter().enumerate() {
        let section_blocks = section
            .block_ids
            .iter()
            .map(|block_id| {
                book.blocks
                    .iter()
                    .find(|block| &block.id == block_id)
                    .ok_or_else(|| {
                        BookforgeError::InvalidInput(format!(
                            "section '{}' references missing block '{}'",
                            section.id.0, block_id.0
                        ))
                    })
            })
            .collect::<Result<Vec<_>>>()?;

        let mut current = Vec::<&Block>::new();
        let mut current_tokens = 0usize;
        let section_segments_start = segments.len();

        for block in section_blocks {
            // pre/code content is layout and syntax, not prose: sending it
            // to the model both mistranslates it and destroys intentional
            // whitespace. Excluded blocks are never patched, so the
            // original markup survives rebuild byte-for-byte.
            if matches!(block.kind, BlockKind::Code | BlockKind::PageFurniture) {
                continue;
            }
            let block_tokens = estimate_tokens(&block_text(block)).max(1);
            let should_flush = !current.is_empty()
                && current_tokens + block_tokens > config.max_segment_tokens
                && !should_keep_with_previous(&current, block);

            if should_flush {
                push_segment(
                    &mut segments,
                    book,
                    section,
                    section_index,
                    &current,
                    config,
                );
                current.clear();
                current_tokens = 0;
            }

            current.push(block);
            current_tokens += block_tokens;
        }

        if !current.is_empty() {
            push_segment(
                &mut segments,
                book,
                section,
                section_index,
                &current,
                config,
            );
        }

        let total_in_section = segments.len() - section_segments_start;
        for (offset, segment) in segments[section_segments_start..].iter_mut().enumerate() {
            segment.metadata.segment_index_in_section = offset;
            segment.metadata.total_segments_in_section = total_in_section;
        }
    }

    apply_context(&mut segments, config.context_tokens);

    Ok(segments)
}

fn push_segment(
    segments: &mut Vec<Segment>,
    book: &Book,
    section: &Section,
    section_index: usize,
    blocks: &[&Block],
    config: &SegmentationConfig,
) {
    let segment_blocks = blocks
        .iter()
        .map(|block| {
            let mut spans = block.protected_spans.clone();
            spans.sort_by(|left, right| {
                left.text
                    .cmp(&right.text)
                    .then_with(|| left.kind.as_str().cmp(right.kind.as_str()))
            });
            spans.dedup();
            SegmentBlock {
                block_id: block.id.clone(),
                kind: block_kind_label(block.kind).to_string(),
                text: block_text(block),
                text_runs: block
                    .text_runs
                    .iter()
                    .map(|run| SegmentTextRun {
                        id: run.id.clone(),
                        text: run.text.clone(),
                    })
                    .collect(),
                protected_spans: spans,
            }
        })
        .collect::<Vec<_>>();
    let source_text = segment_blocks
        .iter()
        .map(|block| block.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let checksum = stable_hash(&source_text);
    let ordinal = segments.len();
    let first_block = blocks
        .first()
        .map(|block| block.id.0.as_str())
        .unwrap_or("empty");
    let id = SegmentId(format!(
        "seg_{}_{}_{}",
        section.id.0,
        first_block,
        &checksum[..12]
    ));

    let mut preserve_spans = blocks
        .iter()
        .flat_map(|block| block.protected_spans.iter().map(|span| span.text.clone()))
        .collect::<Vec<_>>();
    preserve_spans.sort();
    preserve_spans.dedup();

    let mut preserve_markers = blocks
        .iter()
        .flat_map(|block| block.inline_marks.iter().map(|mark| mark.id.clone()))
        .collect::<Vec<_>>();
    preserve_markers.sort();
    preserve_markers.dedup();

    let token_estimate = blocks
        .iter()
        .map(|block| estimate_tokens(&block_text(block)).max(1))
        .sum::<usize>();

    let metadata = SegmentMetadata {
        book_title: book.metadata.title.clone(),
        section_title: section.title.clone(),
        section_index,
        segment_index_in_section: 0,
        total_segments_in_section: 0,
    };

    segments.push(Segment {
        id,
        section_id: section.id.clone(),
        ordinal,
        block_ids: blocks.iter().map(|block| block.id.clone()).collect(),
        source: SegmentSource {
            text: source_text,
            blocks: segment_blocks,
            token_estimate,
        },
        context: SegmentContext::default(),
        metadata,
        constraints: SegmentConstraints {
            preserve_markers,
            preserve_spans,
            max_tokens: config.max_segment_tokens,
        },
        checksum,
    });
}

fn apply_context(segments: &mut [Segment], context_tokens: usize) {
    if context_tokens == 0 {
        return;
    }

    let sources = segments
        .iter()
        .map(|segment| segment.source.text.clone())
        .collect::<Vec<_>>();

    for (index, segment) in segments.iter_mut().enumerate() {
        segment.context.before = index
            .checked_sub(1)
            .and_then(|previous| sources.get(previous))
            .map(|text| tail_words(text, context_tokens));
        segment.context.after = sources
            .get(index + 1)
            .map(|text| head_words(text, context_tokens));
    }
}

fn should_keep_with_previous(current: &[&Block], next: &Block) -> bool {
    let Some(previous) = current.last() else {
        return false;
    };

    matches!(previous.kind, crate::ir::BlockKind::Heading(_))
        && estimate_tokens(&block_text(next)) <= 80
}

fn block_text(block: &Block) -> String {
    block
        .text_runs
        .iter()
        .map(|run| run.text.as_str())
        .collect::<Vec<_>>()
        .join("")
}

fn stable_hash(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to string should not fail");
    }
    output
}

fn head_words(text: &str, max_words: usize) -> String {
    text.split_whitespace()
        .take(max_words)
        .collect::<Vec<_>>()
        .join(" ")
}

fn tail_words(text: &str, max_words: usize) -> String {
    let words = text.split_whitespace().collect::<Vec<_>>();
    let start = words.len().saturating_sub(max_words);
    words[start..].join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{
        BlockKind, BookFormat, BookId, DomPath, Metadata, ProtectedSpanKind, Resource, Section,
        SpineItem, TextRun,
    };

    #[test]
    fn token_estimate_is_derived_from_the_dominant_script() {
        assert_eq!(estimate_tokens("abcdefgh"), 2);
        assert_eq!(estimate_tokens("矛盾是普遍存在的"), 8);
        // Deliberate post-estimator expectation: proportional per-character
        // weighting prices 18 Latin + 18 Han characters at ceil(22.5) = 23,
        // where the retired dominant-class rule counted all 36 chars at one
        // token each and overestimated the Latin half.
        assert_eq!(
            estimate_tokens("Project Gutenberg 矛盾是普遍存在的实践是检验真理的标准"),
            23
        );
        assert_eq!(estimate_tokens("1234"), 1);
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn segmentation_recomputes_stale_block_token_estimates() {
        let mut book = book_with_two_sections();
        book.blocks[1].text_runs[0].text = "矛盾是普遍存在的".to_string();
        book.blocks[1].token_estimate = 1;
        book.blocks[2].text_runs[0].text = "实践是检验真理的标准".to_string();
        book.blocks[2].token_estimate = 1;

        let segments = build_segments(
            &book,
            &SegmentationConfig {
                max_segment_tokens: 12,
                context_tokens: 0,
            },
        )
        .expect("segments should build");

        let section_a = segments
            .iter()
            .filter(|segment| segment.section_id.0 == "sec_000000")
            .collect::<Vec<_>>();
        assert_eq!(section_a.len(), 2);
        assert_eq!(section_a[0].source.token_estimate, 9);
        assert_eq!(section_a[1].source.token_estimate, 10);
    }

    #[test]
    fn builds_stable_segments_without_crossing_sections() {
        let book = book_with_two_sections();
        let config = SegmentationConfig {
            max_segment_tokens: 10,
            context_tokens: 4,
        };

        let first = build_segments(&book, &config).expect("segments should build");
        let second = build_segments(&book, &config).expect("segments should be stable");

        assert_eq!(first.len(), 3);
        assert_eq!(first[0].id, second[0].id);
        assert_eq!(first[1].checksum, second[1].checksum);
        assert_eq!(first[0].section_id.0, "sec_000000");
        assert_eq!(first[1].section_id.0, "sec_000000");
        assert_eq!(first[2].section_id.0, "sec_000001");
        assert_eq!(first[2].block_ids, vec![BlockId("b_000003".to_string())]);
    }

    #[test]
    fn code_blocks_are_excluded_from_segments() {
        let mut book = book_with_two_sections();
        book.blocks[1].kind = BlockKind::Code;
        let config = SegmentationConfig {
            max_segment_tokens: 100,
            context_tokens: 0,
        };

        let segments = build_segments(&book, &config).expect("segments should build");
        let segmented_blocks: Vec<&str> = segments
            .iter()
            .flat_map(|segment| segment.block_ids.iter().map(|id| id.0.as_str()))
            .collect();

        assert!(
            !segmented_blocks.contains(&"b_000001"),
            "code block must not be segmented for translation"
        );
        assert!(segmented_blocks.contains(&"b_000000"));
        assert!(segmented_blocks.contains(&"b_000002"));
    }

    #[test]
    fn rejects_zero_token_limit() {
        let book = book_with_two_sections();
        let config = SegmentationConfig {
            max_segment_tokens: 0,
            context_tokens: 0,
        };

        assert!(build_segments(&book, &config).is_err());
    }

    #[test]
    fn segment_blocks_carry_protected_span_kinds_directly() {
        let spans = vec![
            ProtectedSpan {
                kind: ProtectedSpanKind::Math,
                text: "E=mc^2".to_string(),
            },
            ProtectedSpan {
                kind: ProtectedSpanKind::Url,
                text: "https://example.com".to_string(),
            },
            ProtectedSpan {
                kind: ProtectedSpanKind::Number,
                text: "42".to_string(),
            },
        ];
        let mut book = book_with_two_sections();
        book.blocks[0].protected_spans = spans.clone();

        let segments = build_segments(
            &book,
            &SegmentationConfig {
                max_segment_tokens: 100,
                context_tokens: 0,
            },
        )
        .expect("segments should build");

        let mut expected = spans;
        expected.sort_by(|left, right| {
            left.text
                .cmp(&right.text)
                .then_with(|| left.kind.as_str().cmp(right.kind.as_str()))
        });
        assert_eq!(segments[0].source.blocks[0].protected_spans, expected);
    }

    #[test]
    fn protected_span_metadata_does_not_change_segment_checksum_or_id() {
        let config = SegmentationConfig {
            max_segment_tokens: 100,
            context_tokens: 0,
        };
        let baseline = book_with_two_sections();
        let baseline_segments =
            build_segments(&baseline, &config).expect("baseline segments should build");

        let mut with_span = baseline;
        with_span.blocks[0].protected_spans = vec![ProtectedSpan {
            kind: ProtectedSpanKind::Number,
            text: "1".to_string(),
        }];
        let segments_with_span =
            build_segments(&with_span, &config).expect("segments with spans should build");

        assert_eq!(
            segments_with_span[0].checksum,
            baseline_segments[0].checksum
        );
        assert_eq!(segments_with_span[0].id, baseline_segments[0].id);
    }

    #[test]
    fn cache_namespace_changes_when_segmentation_settings_change() {
        let a = compute_cache_namespace(1200, 160, "Balanced", false, "v1", "glossary:a", "", "");
        let b = compute_cache_namespace(1201, 160, "Balanced", false, "v1", "glossary:a", "", "");
        let c = compute_cache_namespace(1200, 160, "Balanced", true, "v1", "glossary:a", "", "");
        let d = compute_cache_namespace(
            1200,
            160,
            "Balanced",
            false,
            "batch_v1",
            "glossary:a",
            "",
            "",
        );
        let e = compute_cache_namespace(1200, 160, "Balanced", false, "v1", "glossary:a", "", "");
        let f = compute_cache_namespace(1200, 160, "Balanced", false, "v1", "glossary:b", "", "");

        assert_ne!(a, b, "max_segment_tokens must affect namespace");
        assert_ne!(a, c, "batch_enabled must affect namespace");
        assert_ne!(a, d, "prompt_version must affect namespace");
        assert_ne!(a, f, "glossary fingerprint must affect namespace");
        assert_eq!(a, e, "namespace is deterministic for identical inputs");
    }

    #[test]
    fn prompt_only_marker_projection_preserves_cache_identity() {
        let source = "<m1><m2>eyes</m2></m1>";
        let source_checksum = stable_hash(source);
        let projection = crate::marker::collapse_nested_markers_for_prompt(source);
        let namespace =
            compute_cache_namespace(1200, 160, "Balanced", true, "v1", "glossary:a", "", "");

        assert_ne!(projection.text, source);
        assert_ne!(stable_hash(&projection.text), source_checksum);
        assert_eq!(
            stable_hash(source),
            source_checksum,
            "the segment source/checksum remains the unprojected IR text"
        );
        assert_eq!(
            namespace, "60bdecff5342c2a413e077c23b8a647208043258616c5beda9f2fc0ec25c1347",
            "a reversible render projection must not move the existing cache namespace"
        );
    }

    #[test]
    fn legacy_cache_namespace_v1_ignores_glossary_fingerprint() {
        let current_without_terms =
            compute_cache_namespace(1200, 160, "Balanced", false, "v1", "", "", "");
        let current_with_terms =
            compute_cache_namespace(1200, 160, "Balanced", false, "v1", "glossary:a", "", "");
        let legacy = compute_cache_namespace_v1(1200, 160, "Balanced", false, "v1");

        assert_ne!(legacy, current_without_terms);
        assert_ne!(legacy, current_with_terms);
        assert_eq!(
            legacy,
            compute_cache_namespace_v1(1200, 160, "Balanced", false, "v1")
        );
    }

    #[test]
    fn cache_namespace_is_stable_when_style_fingerprint_is_empty() {
        // Users who don't use --style must see no cache invalidation when
        // they upgrade to a build that supports style sheets.
        let without_style =
            compute_cache_namespace(1200, 160, "Balanced", false, "v1", "glossary:a", "", "");
        let still_without_style =
            compute_cache_namespace(1200, 160, "Balanced", false, "v1", "glossary:a", "", "");
        assert_eq!(without_style, still_without_style);
    }

    #[test]
    fn cache_namespace_changes_when_style_fingerprint_changes() {
        let baseline =
            compute_cache_namespace(1200, 160, "Balanced", false, "v1", "glossary:a", "", "");
        let with_style = compute_cache_namespace(
            1200,
            160,
            "Balanced",
            false,
            "v1",
            "glossary:a",
            "style:a",
            "",
        );
        let with_other_style = compute_cache_namespace(
            1200,
            160,
            "Balanced",
            false,
            "v1",
            "glossary:a",
            "style:b",
            "",
        );

        assert_ne!(
            baseline, with_style,
            "switching on a style sheet must invalidate cache"
        );
        assert_ne!(
            with_style, with_other_style,
            "different style fingerprints must yield different namespaces"
        );
    }

    #[test]
    fn cache_namespace_changes_when_entities_fingerprint_changes() {
        let baseline =
            compute_cache_namespace(1200, 160, "Balanced", false, "v1", "glossary:a", "", "");
        let with_entities = compute_cache_namespace(
            1200,
            160,
            "Balanced",
            false,
            "v1",
            "glossary:a",
            "",
            "entities:a",
        );
        let with_other_entities = compute_cache_namespace(
            1200,
            160,
            "Balanced",
            false,
            "v1",
            "glossary:a",
            "",
            "entities:b",
        );
        assert_ne!(
            baseline, with_entities,
            "switching on entities must invalidate cache"
        );
        assert_ne!(
            with_entities, with_other_entities,
            "different entity fingerprints must yield different namespaces"
        );
    }

    #[test]
    fn style_and_entities_fingerprints_use_distinct_domain_separators() {
        // The same hex string used as style vs. entities must not produce
        // the same namespace — domain separators prevent the collision.
        let as_style =
            compute_cache_namespace(1200, 160, "Balanced", false, "v1", "glossary:a", "ab", "");
        let as_entities =
            compute_cache_namespace(1200, 160, "Balanced", false, "v1", "glossary:a", "", "ab");
        assert_ne!(as_style, as_entities);
    }

    fn identity_fixture() -> CacheIdentity {
        CacheIdentity::minimal(MinimalCacheIdentity {
            segment: &test_segment(),
            provider: "openrouter",
            model: "google/gemini-2.5-flash",
            source_lang: Some("English"),
            target_lang: "Italian",
            prompt_version: "batch_v3",
            cache_namespace: "namespace_a",
        })
    }

    fn test_segment() -> Segment {
        Segment {
            id: SegmentId("seg_identity".to_string()),
            section_id: SectionId("sec_000000".to_string()),
            ordinal: 0,
            block_ids: Vec::new(),
            source: SegmentSource {
                text: "Source".to_string(),
                blocks: Vec::new(),
                token_estimate: 2,
            },
            context: SegmentContext::default(),
            metadata: SegmentMetadata::default(),
            constraints: SegmentConstraints::default(),
            checksum: "checksum_identity".to_string(),
        }
    }

    #[test]
    fn cache_identity_is_deterministic() {
        assert_eq!(
            identity_fixture().fingerprint(),
            identity_fixture().fingerprint()
        );
    }

    #[test]
    fn cache_identity_is_sensitive_to_every_field() {
        let baseline = identity_fixture();
        let baseline_fp = baseline.fingerprint();
        let mut altered = Vec::new();

        let mut tweak = identity_fixture();
        tweak.schema_version += 1;
        altered.push(("schema_version", tweak));

        let mut tweak = identity_fixture();
        tweak.source_hash = "other_hash".to_string();
        altered.push(("source_hash", tweak));

        let mut tweak = identity_fixture();
        tweak.provider = "deepseek".to_string();
        altered.push(("provider", tweak));

        let mut tweak = identity_fixture();
        tweak.model = "deepseek-v4-flash".to_string();
        altered.push(("model", tweak));

        let mut tweak = identity_fixture();
        tweak.source_lang = Some("French".to_string());
        altered.push(("source_lang", tweak));

        let mut tweak = identity_fixture();
        tweak.target_lang = "French".to_string();
        altered.push(("target_lang", tweak));

        let mut tweak = identity_fixture();
        tweak.prompt_version = "v2".to_string();
        altered.push(("prompt_version", tweak));

        let mut tweak = identity_fixture();
        tweak.cache_namespace = "namespace_b".to_string();
        altered.push(("cache_namespace", tweak));

        let mut tweak = identity_fixture();
        tweak.prompt_extra = Some("be brief".to_string());
        altered.push(("prompt_extra", tweak));

        let mut tweak = identity_fixture();
        tweak.max_segment_tokens = 2_500;
        altered.push(("max_segment_tokens", tweak));

        let mut tweak = identity_fixture();
        tweak.context_tokens = 80;
        altered.push(("context_tokens", tweak));

        let mut tweak = identity_fixture();
        tweak.context_window = 4;
        altered.push(("context_window", tweak));

        let mut tweak = identity_fixture();
        tweak.context_budget_tokens = 400;
        altered.push(("context_budget_tokens", tweak));

        let mut tweak = identity_fixture();
        tweak.context_scope = crate::config::ContextScope::Book;
        altered.push(("context_scope", tweak));

        let mut tweak = identity_fixture();
        tweak.strict_context = Some(false);
        altered.push(("strict_context_loose", tweak));

        let mut tweak = identity_fixture();
        tweak.strict_context = Some(true);
        altered.push(("strict_context_strict", tweak));

        let mut tweak = identity_fixture();
        tweak.profile = crate::config::TranslationProfile::Safe;
        altered.push(("profile", tweak));

        let mut tweak = identity_fixture();
        tweak.batch_enabled = true;
        altered.push(("batch_enabled", tweak));

        let mut tweak = identity_fixture();
        tweak.batch_target_tokens = 16_000;
        altered.push(("batch_target_tokens", tweak));

        let mut tweak = identity_fixture();
        tweak.batch_max_items = 64;
        altered.push(("batch_max_items", tweak));

        let mut tweak = identity_fixture();
        tweak.batch_adaptive_sizing = true;
        altered.push(("batch_adaptive_sizing", tweak));

        let mut tweak = identity_fixture();
        tweak.batch_split_on_json_failure = true;
        altered.push(("batch_split_on_json_failure", tweak));

        let mut tweak = identity_fixture();
        tweak.batch_repair_invalid_items = true;
        altered.push(("batch_repair_invalid_items", tweak));

        let mut tweak = identity_fixture();
        tweak.compact_prompts = true;
        altered.push(("compact_prompts", tweak));

        let mut tweak = identity_fixture();
        tweak.glossary_fingerprint = "glossary:a".to_string();
        altered.push(("glossary_fingerprint", tweak));

        let mut tweak = identity_fixture();
        tweak.style_fingerprint = "style:a".to_string();
        altered.push(("style_fingerprint", tweak));

        let mut tweak = identity_fixture();
        tweak.entities_fingerprint = "entities:a".to_string();
        altered.push(("entities_fingerprint", tweak));

        let mut tweak = identity_fixture();
        tweak.context_before = "Neighbor before".to_string();
        altered.push(("context_before", tweak));

        let mut tweak = identity_fixture();
        tweak.context_after = "Neighbor after".to_string();
        altered.push(("context_after", tweak));

        let mut tweak = identity_fixture();
        tweak.glossary_rendered = "glossary:a".to_string();
        altered.push(("glossary_rendered", tweak));

        let mut tweak = identity_fixture();
        tweak.style_rendered = "style block".to_string();
        altered.push(("style_rendered", tweak));

        let mut tweak = identity_fixture();
        tweak.entities_rendered = "entity block".to_string();
        altered.push(("entities_rendered", tweak));

        let mut tweak = identity_fixture();
        tweak.bilingual_mode = crate::config::BilingualMode::AppendText;
        altered.push(("bilingual_mode", tweak));

        let mut tweak = identity_fixture();
        tweak.bilingual_separator = "\n\n".to_string();
        altered.push(("bilingual_separator", tweak));

        let mut tweak = identity_fixture();
        tweak.bilingual_style = crate::config::BilingualStyle::Prominent;
        altered.push(("bilingual_style", tweak));

        let mut tweak = identity_fixture();
        tweak.thinking_disabled = true;
        altered.push(("thinking_disabled", tweak));

        let mut tweak = identity_fixture();
        tweak.max_output_tokens = Some(2048);
        altered.push(("max_output_tokens", tweak));

        let mut tweak = identity_fixture();
        tweak.batch_max_output_tokens = Some(8192);
        altered.push(("batch_max_output_tokens", tweak));

        let mut tweak = identity_fixture();
        tweak.json_mode = crate::config::JsonMode::ResponseFormat;
        altered.push(("json_mode", tweak));

        let mut failures = Vec::new();
        for (name, identity) in altered {
            if identity.fingerprint() == baseline_fp {
                failures.push(name);
            }
        }
        assert!(
            failures.is_empty(),
            "cache identity must be sensitive to every field; insensitive: {failures:?}"
        );
    }

    #[test]
    fn cache_identity_distinguishes_unknown_strict_context() {
        // A legacy run (None) must never match an explicit strict or loose
        // choice, and the two explicit choices must differ from each other.
        let unknown = identity_fixture().fingerprint();
        let mut loose = identity_fixture();
        loose.strict_context = Some(false);
        let mut strict = identity_fixture();
        strict.strict_context = Some(true);
        assert_ne!(unknown, loose.fingerprint());
        assert_ne!(unknown, strict.fingerprint());
        assert_ne!(loose.fingerprint(), strict.fingerprint());
    }

    #[test]
    fn cache_identity_fingerprint_bumps_with_schema_version() {
        let baseline = identity_fixture().fingerprint();
        let mut bumped = identity_fixture();
        bumped.schema_version += 1;
        assert_ne!(baseline, bumped.fingerprint());
    }

    #[test]
    fn cache_identity_distinguishes_same_source_in_different_contexts() {
        // The same source text at a different position (different ordered
        // neighbor/context content) must never collide: identical checksum,
        // different fingerprint.
        let mut seg_a = test_segment();
        seg_a.source.text = "identical text".to_string();
        seg_a.checksum = stable_hash(&seg_a.source.text);
        seg_a.context.before = Some("before A".to_string());
        seg_a.context.after = Some("after A".to_string());
        let mut seg_b = seg_a.clone();
        seg_b.context.before = Some("before B".to_string());
        seg_b.context.after = Some("after B".to_string());

        let identity = |segment: &Segment| {
            CacheIdentity::minimal(MinimalCacheIdentity {
                segment,
                provider: "openrouter",
                model: "google/gemini-2.5-flash",
                source_lang: Some("English"),
                target_lang: "Italian",
                prompt_version: "batch_v3",
                cache_namespace: "namespace_a",
            })
        };

        let a = identity(&seg_a);
        let b = identity(&seg_b);
        assert_eq!(a.source_hash, b.source_hash, "source text is identical");
        assert_ne!(
            a.fingerprint(),
            b.fingerprint(),
            "different neighbor content must not share a cache identity"
        );
    }

    fn book_with_two_sections() -> Book {
        let section_a = SectionId("sec_000000".to_string());
        let section_b = SectionId("sec_000001".to_string());

        Book {
            source_path: None,
            id: BookId("test".to_string()),
            format: BookFormat::Epub,
            metadata: Metadata::default(),
            manifest: vec![Resource {
                id: "chapter".to_string(),
                href: "chapter.xhtml".to_string(),
                media_type: "application/xhtml+xml".to_string(),
                properties: Vec::new(),
            }],
            spine: vec![SpineItem {
                idref: "chapter".to_string(),
                href: Some("chapter.xhtml".to_string()),
            }],
            sections: vec![
                Section {
                    id: section_a.clone(),
                    href: "chapter.xhtml".to_string(),
                    spine_index: 0,
                    title: Some("One".to_string()),
                    heading_level: Some(1),
                    block_ids: vec![
                        BlockId("b_000000".to_string()),
                        BlockId("b_000001".to_string()),
                        BlockId("b_000002".to_string()),
                    ],
                    prev: None,
                    next: Some(section_b.clone()),
                },
                Section {
                    id: section_b.clone(),
                    href: "chapter2.xhtml".to_string(),
                    spine_index: 1,
                    title: None,
                    heading_level: None,
                    block_ids: vec![BlockId("b_000003".to_string())],
                    prev: Some(section_a.clone()),
                    next: None,
                },
            ],
            blocks: vec![
                block("b_000000", &section_a, BlockKind::Heading(1), "One", 2),
                block(
                    "b_000001",
                    &section_a,
                    BlockKind::Paragraph,
                    "short lead",
                    3,
                ),
                block(
                    "b_000002",
                    &section_a,
                    BlockKind::Paragraph,
                    "this paragraph forces a second segment",
                    10,
                ),
                block(
                    "b_000003",
                    &section_b,
                    BlockKind::Paragraph,
                    "new section must stay separate",
                    4,
                ),
            ],
        }
    }

    fn block(
        id: &str,
        section_id: &SectionId,
        kind: BlockKind,
        text: &str,
        token_estimate: usize,
    ) -> Block {
        Block {
            id: BlockId(id.to_string()),
            section_id: section_id.clone(),
            kind,
            dom_path: DomPath(vec![0]),
            text_runs: vec![TextRun {
                id: "r0".to_string(),
                text: text.to_string(),
            }],
            inline_marks: Vec::new(),
            protected_spans: Vec::new(),
            token_estimate,
        }
    }
}
