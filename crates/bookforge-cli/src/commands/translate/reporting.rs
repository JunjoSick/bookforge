use anyhow::Result;
use bookforge_core::{
    config::TranslationConfig,
    segment::{BlockTranslation, Segment},
};
use bookforge_epub::rebuild_epub;
use bookforge_llm::{QaSegmentReview, SegmentTranslation};
use bookforge_store::{JobRecord, JobStore};

use crate::{
    cost::estimate_cost_usd,
    performance::performance_summary_from_events,
    report::{ReportInput, write_report},
};

pub fn block_translations(translations: &[SegmentTranslation]) -> Vec<BlockTranslation> {
    translations
        .iter()
        .flat_map(|translation| translation.blocks.iter().cloned())
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub fn print_summary_rebuild_and_report(
    store: &JobStore,
    job: &JobRecord,
    book: &bookforge_core::ir::Book,
    segments: &[Segment],
    translations: &[SegmentTranslation],
    qa_reviews: &[QaSegmentReview],
    config: &TranslationConfig,
    print_stdout: bool,
) -> Result<()> {
    let block_translations = block_translations(translations);
    rebuild_epub(book, &block_translations, &config.output)?;
    let summary = store
        .summary(&job.id)?
        .ok_or_else(|| anyhow::anyhow!("job '{}' was not found after translation", job.id))?;
    let report_job = store
        .get_job(&job.id)?
        .ok_or_else(|| anyhow::anyhow!("job '{}' was not found after translation", job.id))?;
    let segment_records = store.segment_records(&job.id)?;
    let performance = report_job
        .events_path
        .as_ref()
        .and_then(|path| performance_summary_from_events(path).ok().flatten());
    let report = write_report(ReportInput {
        job: &report_job,
        summary: &summary,
        segments,
        segment_records: &segment_records,
        translations,
        qa_reviews,
        performance,
        output: &config.output,
    })?;
    store.update_job_report_paths(&job.id, &report.json, &report.markdown)?;

    if print_stdout {
        println!(
            "Translated: {}/{} segments",
            summary.succeeded, summary.total_segments
        );
        println!("Cached: {}", summary.cached);
        println!("Retried: {}", summary.retried);
        println!("Needs review: {}", summary.needs_review);
        println!("Failed: {}", summary.failed);
        println!("Input tokens: {}", summary.input_tokens);
        println!("Output tokens: {}", summary.output_tokens);
        if let Some(cost) = estimate_cost_usd(
            &job.provider,
            &job.model,
            summary.input_tokens,
            summary.output_tokens,
        ) {
            println!("Estimated cost: ${cost:.6}");
        }
        println!("Output: {}", config.output.display());
        println!("Report: {}", report.markdown.display());
    }

    Ok(())
}
