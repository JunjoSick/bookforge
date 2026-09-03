use anyhow::Result;
use bookforge_core::segment::{Segment, SegmentStatus};
use bookforge_llm::SegmentTranslation;
use bookforge_store::{JobStore, SaveCachedTranslation};

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
