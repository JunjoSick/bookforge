use anyhow::Result;
use bookforge_core::{
    RunConfigSnapshot,
    config::TranslationConfig,
    segment::{BlockTranslation, Segment, SegmentStatus},
};
use bookforge_epub::{RebuildOptions, rebuild_epub_with_options};
use bookforge_llm::{QaSegmentReview, SegmentTranslation};
use bookforge_store::{JobRecord, JobStore};

use crate::{
    commands::validate,
    cost::estimate_cost_usd_with_cached,
    performance::performance_summary_from_events,
    report::{ReportFiles, ReportInput, TranslationQaInput, write_report},
};

pub fn block_translations(translations: &[SegmentTranslation]) -> Vec<BlockTranslation> {
    translations
        .iter()
        .flat_map(|translation| translation.blocks.iter().cloned())
        .collect()
}

/// End-of-run stdout summary shared by `translate` and `resume` (UI-31).
/// The two commands used to print this block verbatim-duplicated and had
/// already drifted once; now it exists exactly once.
pub(crate) fn print_run_summary(
    summary: &bookforge_store::JobSummary,
    provider: &str,
    model: &str,
    output: &std::path::Path,
    report_markdown: &std::path::Path,
    review_job_id: &str,
) {
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
    if let Some(cost) = estimate_cost_usd_with_cached(
        provider,
        model,
        summary.input_tokens,
        summary.input_cached_tokens,
        summary.output_tokens,
    ) {
        println!("Estimated cost: ${cost:.6}");
    }
    println!("Output: {}", output.display());
    println!("Report: {}", report_markdown.display());
    println!("Review: bookforge review {review_job_id} --open");
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
    rebuild_options: &RebuildOptions,
    validate_output: bool,
    strict_epubcheck: bool,
    print_stdout: bool,
) -> Result<()> {
    let block_translations = block_translations(translations);
    rebuild_epub_with_options(book, &block_translations, &config.output, rebuild_options)?;
    let mut validation_failure = None;
    if validate_output || strict_epubcheck {
        let validation_path = validate::default_report_path(&config.output);
        let validation =
            validate::validate_and_write(&config.output, &validation_path, strict_epubcheck)?;
        if print_stdout {
            println!("Validation report: {}", validation_path.display());
        }
        if validation.failed {
            store.mark_job_failed(&job.id)?;
            validation_failure = Some(validation_path);
        }
    }

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
    let qa_inputs = translations
        .iter()
        .map(|translation| {
            TranslationQaInput::new(
                translation.segment_id.0.clone(),
                matches!(
                    translation.status,
                    SegmentStatus::Succeeded | SegmentStatus::SkippedCached
                ),
                translation.joined_text(),
            )
        })
        .collect::<Vec<_>>();
    let corrected_segments = store
        .load_terminal_segment_translations(&job.id)?
        .iter()
        .filter(|translation| translation.human_corrected)
        .count();
    let report = write_report(ReportInput {
        job: &report_job,
        summary: &summary,
        segments,
        segment_records: &segment_records,
        translations: &qa_inputs,
        qa_reviews,
        performance,
        output: &config.output,
        corrected_segments,
    })?;
    store.update_job_report_paths(&job.id, &report.json, &report.markdown)?;

    if let Some(validation_path) = validation_failure {
        anyhow::bail!(
            "rebuilt EPUB failed validation; see {}",
            validation_path.display()
        );
    }

    if print_stdout {
        print_run_summary(
            &summary,
            &job.provider,
            &job.model,
            &config.output,
            &report.markdown,
            &job.id,
        );
    }

    Ok(())
}

/// Refreshes the on-disk QA report (`{stem}.report.json` / `.report.md`) for
/// a job entirely from store-backed data, without requiring the in-memory
/// run results (`SegmentTranslation`, `QaSegmentReview`) that only exist
/// during a live translate/resume run. Used after a manual correction, whose
/// invocation is a separate process from the original run — the correction
/// itself is already durably persisted by the time this runs, so this is a
/// best-effort refresh of a secondary artifact, not part of the correction's
/// atomicity contract.
///
/// QA-review-derived warnings (from an optional double-check pass) cannot be
/// reconstructed here since they are never persisted to the store; the
/// refreshed report simply omits them, which is correct because a segment
/// that was just manually corrected has no meaningful stale AI QA verdict to
/// preserve anyway.
pub(crate) fn regenerate_report_after_correction(
    store: &JobStore,
    job: &JobRecord,
    segments: &[Segment],
) -> Result<ReportFiles> {
    let summary = store
        .summary(&job.id)?
        .ok_or_else(|| anyhow::anyhow!("job '{}' was not found after correction", job.id))?;
    let report_job = store
        .get_job(&job.id)?
        .ok_or_else(|| anyhow::anyhow!("job '{}' was not found after correction", job.id))?;
    let segment_records = store.segment_records(&job.id)?;
    let stored_translations = store.load_terminal_segment_translations(&job.id)?;
    let corrected_segments = stored_translations
        .iter()
        .filter(|translation| translation.human_corrected)
        .count();
    let qa_inputs = stored_translations
        .iter()
        .map(|translation| {
            TranslationQaInput::new(
                translation.segment_id.clone(),
                matches!(translation.status.as_str(), "succeeded" | "skipped_cached"),
                translation.translated_text.clone(),
            )
        })
        .collect::<Vec<_>>();
    let performance = report_job
        .events_path
        .as_ref()
        .and_then(|path| performance_summary_from_events(path).ok().flatten());
    let report = write_report(ReportInput {
        job: &report_job,
        summary: &summary,
        segments,
        segment_records: &segment_records,
        translations: &qa_inputs,
        qa_reviews: &[],
        performance,
        output: &report_job.output_path,
        corrected_segments,
    })?;
    store.update_job_report_paths(&job.id, &report.json, &report.markdown)?;
    Ok(report)
}

pub fn rebuild_options_from_snapshot(snapshot: &RunConfigSnapshot) -> RebuildOptions {
    RebuildOptions {
        target_language: Some(snapshot.target_language.clone()),
        creator: snapshot.creator.clone(),
        mode: snapshot.bilingual_mode,
        bilingual_separator: snapshot.bilingual_separator.clone(),
        bilingual_style: snapshot.bilingual_style,
        bilingual_css: snapshot.bilingual_css.clone(),
    }
}
