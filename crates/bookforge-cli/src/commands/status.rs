use clap::Args;

use std::path::PathBuf;

use bookforge_core::RunConfigSnapshot;
use bookforge_store::{JobRecord, JobStore, JobSummary};

use crate::{
    performance::{RunPerformanceSummary, performance_summary_from_events},
    report::report_paths,
};

#[derive(Debug, Args)]
pub struct StatusArgs {
    pub job_id: String,
}

pub async fn run(args: StatusArgs) -> anyhow::Result<()> {
    let store = JobStore::open_default()?;
    let Some(job) = store.get_job(&args.job_id)? else {
        anyhow::bail!("job '{}' was not found", args.job_id);
    };

    let Some(summary) = store.summary(&args.job_id)? else {
        anyhow::bail!("job '{}' summary unavailable", args.job_id);
    };
    let snapshot = store.load_job_config_snapshot(&args.job_id)?;

    println!("Job: {}", summary.id);
    println!("Status: {}", summary.status);
    println!();
    println!("Input: {}", job.input_path.display());
    println!("Output: {}", job.output_path.display());
    println!("Source: {}", job.source_lang.as_deref().unwrap_or("auto"));
    println!("Target: {}", job.target_lang);
    println!();
    println!("Provider: {}", job.provider);
    println!("Model: {}", job.model);
    if let Some(ref base_url) = job.base_url {
        println!("Base URL: {base_url}");
    }
    if let Some(ref api_key_env) = job.api_key_env {
        println!("API key env: {api_key_env}");
    }
    println!();
    println!("Segments:");
    println!("  total:       {}", summary.total_segments);
    println!("  succeeded:   {}", summary.succeeded);
    println!("  cached:      {}", summary.cached);
    println!("  needs review: {}", summary.needs_review);
    println!("  failed:      {}", summary.failed);
    println!("  retry pending: {}", summary.retry_pending);
    println!();
    println!("Tokens:");
    println!("  input:  {}", summary.input_tokens);
    println!("  output: {}", summary.output_tokens);
    println!();
    println!("Retried segments: {}", summary.retried);

    let event_log_path = event_log_path_for_status(&job, snapshot.as_ref(), &args.job_id);
    if event_log_path.exists() {
        println!("Event log: {}", event_log_path.display());
    }
    let report_path = report_path_for_status(&job, snapshot.as_ref());
    if report_path.exists() {
        println!("Report: {}", report_path.display());
    }

    println!();
    println!("Performance:");
    match performance_summary_from_events(&event_log_path)? {
        Some(perf) => print_performance(&perf),
        None => println!("  unavailable: no event log found"),
    }

    match guidance_for_summary(&summary) {
        Some(StatusGuidance::Resume) => println!("Resume: bookforge resume {}", args.job_id),
        Some(StatusGuidance::Review) => {
            println!(
                "Review: segments need manual review; default resume skips needs-review segments."
            );
        }
        None => {}
    }

    Ok(())
}

fn event_log_path_for_status(
    job: &JobRecord,
    snapshot: Option<&RunConfigSnapshot>,
    job_id: &str,
) -> PathBuf {
    job.events_path
        .clone()
        .or_else(|| snapshot.and_then(|snapshot| snapshot.events_path.clone()))
        .unwrap_or_else(|| PathBuf::from(format!(".bookforge/runs/{job_id}/events.jsonl")))
}

fn report_path_for_status(job: &JobRecord, snapshot: Option<&RunConfigSnapshot>) -> PathBuf {
    let fallback_reports = report_paths(&job.output_path);
    job.report_markdown_path
        .clone()
        .or_else(|| snapshot.and_then(|snapshot| snapshot.report_markdown_path.clone()))
        .unwrap_or(fallback_reports.markdown)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusGuidance {
    Resume,
    Review,
}

fn guidance_for_summary(summary: &JobSummary) -> Option<StatusGuidance> {
    if summary.failed > 0 || summary.retry_pending > 0 || summary.status == "interrupted" {
        Some(StatusGuidance::Resume)
    } else if summary.needs_review > 0 {
        Some(StatusGuidance::Review)
    } else {
        None
    }
}

fn print_performance(perf: &RunPerformanceSummary) {
    println!("  requests: {}", perf.request_count);
    println!(
        "  latency p50/p95: {}/{} ms",
        fmt_opt(perf.p50_latency_ms),
        fmt_opt(perf.p95_latency_ms)
    );
    println!("  retries: {}", perf.retries);
    println!(
        "  429/timeouts/server errors: {}/{}/{}",
        perf.rate_limited, perf.timeouts, perf.server_errors
    );
    println!(
        "  invalid/truncated: {}/{}",
        perf.invalid_responses, perf.truncations
    );
    println!("  checkpoint flushes: {}", perf.checkpoint_flushes);
    if let Some(rate) = perf.blocks_per_minute {
        println!("  blocks/min: {rate:.2}");
    } else {
        println!("  blocks/min: n/a");
    }
}

fn fmt_opt(value: Option<u64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "n/a".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job() -> JobRecord {
        JobRecord {
            id: "job_test".to_string(),
            input_path: PathBuf::from("input.epub"),
            input_snapshot_path: None,
            input_sha256: None,
            output_path: PathBuf::from("/tmp/bookforge-status/out.epub"),
            input_hash: "hash".to_string(),
            source_lang: Some("English".to_string()),
            target_lang: "Italian".to_string(),
            provider: "mock".to_string(),
            model: "mock-prefix-target".to_string(),
            base_url: None,
            api_key_env: None,
            status: "succeeded".to_string(),
            events_path: None,
            report_json_path: None,
            report_markdown_path: None,
            book_id: None,
            series_id: None,
        }
    }

    fn snapshot_with_paths(
        events_path: Option<PathBuf>,
        report_markdown_path: Option<PathBuf>,
    ) -> RunConfigSnapshot {
        let settings = bookforge_core::TranslationProfile::V1Fast.resolve();
        RunConfigSnapshot {
            input_path: PathBuf::from("input.epub"),
            input_snapshot_path: None,
            input_sha256: None,
            output_path: PathBuf::from("/tmp/bookforge-status/out.epub"),
            events_path,
            report_json_path: None,
            report_markdown_path,
            source_language: Some("English".to_string()),
            target_language: "Italian".to_string(),
            provider: "mock".to_string(),
            model: "mock-prefix-target".to_string(),
            base_url: None,
            api_key_env: None,
            profile: settings.profile,
            provider_preset: None,
            prompt_version: "v1".to_string(),
            cache_namespace: "cache".to_string(),
            book_id: None,
            series_id: None,
            glossary_budget_tokens: 800,
            glossary_format: bookforge_core::GlossaryFormat::Json,
            prompt_extra: None,
            glossary_fingerprint: String::new(),
            glossary_terms: Vec::new(),
            context_window: 0,
            context_budget_tokens: 1200,
            context_scope: bookforge_core::config::ContextScope::Chapter,
            settings: bookforge_core::ResolvedRunSettingsSnapshot::from_settings(&settings),
        }
    }

    #[test]
    fn status_reports_actual_output_adjacent_report_path() {
        let job = job();

        assert_eq!(
            report_path_for_status(&job, None),
            PathBuf::from("/tmp/bookforge-status/out.report.md")
        );
    }

    #[test]
    fn status_uses_stored_event_path_when_present() {
        let mut job = job();
        job.events_path = Some(PathBuf::from("/tmp/stored-events.jsonl"));
        let snapshot = snapshot_with_paths(Some(PathBuf::from("/tmp/snapshot-events.jsonl")), None);

        assert_eq!(
            event_log_path_for_status(&job, Some(&snapshot), &job.id),
            PathBuf::from("/tmp/stored-events.jsonl")
        );
    }

    #[test]
    fn status_uses_snapshot_event_path_when_job_event_path_missing() {
        let job = job();
        let snapshot = snapshot_with_paths(Some(PathBuf::from("/tmp/snapshot-events.jsonl")), None);

        assert_eq!(
            event_log_path_for_status(&job, Some(&snapshot), &job.id),
            PathBuf::from("/tmp/snapshot-events.jsonl")
        );
    }

    #[test]
    fn status_prints_resume_when_failed_segments_exist_even_if_job_status_needs_review() {
        let summary = JobSummary {
            id: "job_test".to_string(),
            status: "needs_review".to_string(),
            failed: 1,
            ..JobSummary::default()
        };

        assert_eq!(guidance_for_summary(&summary), Some(StatusGuidance::Resume));
    }

    #[test]
    fn status_prints_resume_when_retry_pending_segments_exist() {
        let summary = JobSummary {
            id: "job_test".to_string(),
            status: "needs_review".to_string(),
            retry_pending: 1,
            ..JobSummary::default()
        };

        assert_eq!(guidance_for_summary(&summary), Some(StatusGuidance::Resume));
    }

    #[test]
    fn status_does_not_print_resume_for_only_needs_review_by_default() {
        let summary = JobSummary {
            id: "job_test".to_string(),
            status: "needs_review".to_string(),
            needs_review: 1,
            ..JobSummary::default()
        };

        assert_eq!(guidance_for_summary(&summary), Some(StatusGuidance::Review));
    }
}
