use std::collections::HashMap;

use anyhow::Result;
use bookforge_core::finding::{EngineFinding, QaFindingKind};
use bookforge_core::ir::QaFindingSeverity;
use bookforge_core::segment::{BlockTranslation, Segment, SegmentStatus};
use bookforge_llm::{SegmentTranslation, blocking_source_copy_error, should_validate_source_copy};
use bookforge_store::{JobStore, SaveCachedTranslation, SegmentStatus as StoredStatus};

#[derive(Clone, Copy)]
pub struct CacheContext<'a> {
    pub store: &'a JobStore,
    pub job_id: &'a str,
    pub prompt_version: &'a str,
    pub provider: &'a str,
    pub model: &'a str,
    pub source_lang: Option<&'a str>,
    pub target_lang: &'a str,
    pub cache_namespace: &'a str,
}

/// Apply cross-job cache hits to the eligible `segments`.
///
/// `all_segments` is the FULL ordered segment set of the current run: the
/// expected cache identity is computed over it because per-segment glossary
/// selection depends on the ordered neighborhood/window, and a sparse subset
/// (e.g. the pending candidates passed on resume) would select different terms
/// than the original run and compute different fingerprints. `segments` is the
/// subset the lookup is restricted to (pending, cacheable candidates).
pub fn apply_cached_translations(
    all_segments: &[Segment],
    segments: &[Segment],
    cache: CacheContext<'_>,
) -> Result<Vec<SegmentTranslation>> {
    let mut cached = Vec::new();

    // The expected structured cache identity fingerprint is computed from the
    // job's persisted run snapshot + cache policy over the FULL ordered
    // segment set (never discovered from a prior segment row) and passed
    // straight into the lookup for the eligible candidates only.
    let expected_fingerprints = cache.store.expected_cache_fingerprints(
        cache.job_id,
        all_segments,
        segments,
        cache.provider,
        cache.model,
        cache.prompt_version,
        cache.cache_namespace,
    )?;

    let request = bookforge_store::CacheLookupRequest {
        prompt_version: cache.prompt_version,
        provider: cache.provider,
        model: cache.model,
        source_lang: cache.source_lang,
        target_lang: cache.target_lang,
        cache_namespace: cache.cache_namespace,
        expected_fingerprints: &expected_fingerprints,
    };

    let hits = cache
        .store
        .find_cached_translations_batch(segments, request)?;

    for segment in segments {
        let Some(hit) = hits.get(&segment.id.0) else {
            continue;
        };
        // Cached output may predate the current source-copy rule. A hit that
        // fresh batch output could not pass becomes an ordinary miss; the job
        // that produced it is untouched. The lookup never offers manual
        // corrections, so this only ever rejects provider output.
        if !stored_source_copies(
            segment,
            &hit.blocks,
            cache.provider,
            cache.source_lang,
            cache.target_lang,
        )
        .is_empty()
        {
            continue;
        }
        cache.store.save_cached_translation(SaveCachedTranslation {
            job_id: cache.job_id,
            segment_id: &segment.id.0,
            translated_text: &hit.translated_text,
            blocks: &hit.blocks,
            provider: cache.provider,
            model: cache.model,
            prompt_version: cache.prompt_version,
        })?;
        cached.push(SegmentTranslation {
            segment_id: segment.id.clone(),
            ordinal: segment.ordinal,
            block_ids: segment.block_ids.clone(),
            blocks: hit.blocks.clone(),
            checksum: segment.checksum.clone(),
            status: SegmentStatus::SkippedCached,
            template: "cached".to_string(),
            error: None,
            input_tokens: None,
            input_cached_tokens: None,
            output_tokens: None,
            tokens_estimated: false,
            findings: Vec::new(),
        });
    }
    Ok(cached)
}

/// Blocking source copies among a stored result's `blocks`, judged by the
/// rule batch acceptance applies to fresh output.
fn stored_source_copies(
    segment: &Segment,
    blocks: &[BlockTranslation],
    provider: &str,
    source_lang: Option<&str>,
    target_lang: &str,
) -> Vec<EngineFinding> {
    if !should_validate_source_copy(provider, source_lang, target_lang) {
        return Vec::new();
    }
    let by_id = blocks
        .iter()
        .map(|block| (&block.block_id, block.text.as_str()))
        .collect::<HashMap<_, _>>();
    segment
        .source
        .blocks
        .iter()
        .filter_map(|source| {
            let translation = by_id.get(&source.block_id)?;
            let message = blocking_source_copy_error(
                &source.kind,
                &source.text,
                translation,
                segment.metadata.section_title.as_deref(),
            )?;
            Some(
                EngineFinding::new(QaFindingKind::SourceCopyUnchanged, message)
                    .with_block_id(source.block_id.0.clone())
                    .with_severity(QaFindingSeverity::Error),
            )
        })
        .collect()
}

/// Recheck provider output accepted by an earlier run against the current
/// source-copy rule. A blocking copy moves its segment to `needs_review`
/// with the saved text, blocks, usage and provenance untouched; ordinary
/// resume does not retranslate it. Human-corrected segments are
/// authoritative and never rechecked. Returns how many were reclassified.
pub(crate) fn revalidate_stored_translations(
    store: &JobStore,
    job_id: &str,
    segments: &[Segment],
    source_lang: Option<&str>,
    target_lang: &str,
) -> Result<usize> {
    let by_id = segments
        .iter()
        .map(|segment| (segment.id.0.as_str(), segment))
        .collect::<HashMap<_, _>>();
    let mut reclassified = 0;
    for saved in store.load_terminal_segment_translations(job_id)? {
        if saved.human_corrected
            || !matches!(
                saved.segment_status(),
                StoredStatus::Succeeded | StoredStatus::SkippedCached
            )
        {
            continue;
        }
        let Some(segment) = by_id.get(saved.segment_id.as_str()) else {
            continue;
        };
        let copies = stored_source_copies(
            segment,
            &saved.blocks,
            &saved.provider,
            source_lang,
            target_lang,
        );
        if copies.is_empty() {
            continue;
        }
        let error = format!(
            "saved translation failed source-copy revalidation: {}",
            copies
                .iter()
                .map(|finding| format!(
                    "block {}: {}",
                    finding.block_id.as_deref().unwrap_or_default(),
                    finding.message
                ))
                .collect::<Vec<_>>()
                .join("; ")
        );
        reclassified += usize::from(store.mark_stored_translation_for_review(
            job_id,
            &saved.segment_id,
            &saved.translated_text,
            &error,
            &copies,
        )?);
    }
    Ok(reclassified)
}

pub fn pending_segments_for_job(
    store: &JobStore,
    job_id: &str,
    segments: &[Segment],
) -> Result<Vec<Segment>> {
    let pending_ids = store.pending_segment_ids(job_id)?;
    let pending = pending_ids
        .iter()
        .map(String::as_str)
        .collect::<std::collections::HashSet<_>>();
    Ok(segments
        .iter()
        .filter(|segment| pending.contains(segment.id.0.as_str()))
        .cloned()
        .collect())
}

#[cfg(test)]
mod tests;
