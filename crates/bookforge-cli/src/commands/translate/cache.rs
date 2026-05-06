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

pub fn apply_cached_translations(
    segments: &[Segment],
    cache: CacheContext<'_>,
) -> Result<Vec<SegmentTranslation>> {
    let mut cached = Vec::new();

    let request = bookforge_store::CacheLookupRequest {
        prompt_version: cache.prompt_version,
        provider: cache.provider,
        model: cache.model,
        source_lang: cache.source_lang,
        target_lang: cache.target_lang,
        cache_namespace: cache.cache_namespace,
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
            output_tokens: None,
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
