use clap::Args;

use bookforge_store::JobStore;

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

    let event_log_path = job
        .events_path
        .clone()
        .or_else(|| {
            snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.events_path.clone())
        })
        .unwrap_or_else(|| {
            std::path::PathBuf::from(format!(".bookforge/runs/{}/events.jsonl", args.job_id))
        });
    if event_log_path.exists() {
        println!("Event log: {}", event_log_path.display());
    }
    let fallback_reports = report_paths(&job.output_path);
    let report_path = job
        .report_markdown_path
        .clone()
        .or_else(|| {
            snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.report_markdown_path.clone())
        })
        .unwrap_or(fallback_reports.markdown);
    if report_path.exists() {
        println!("Report: {}", report_path.display());
    }

    println!();
    println!("Performance:");
    match performance_summary_from_events(&event_log_path)? {
        Some(perf) => print_performance(&perf),
        None => println!("  unavailable: no event log found"),
    }

    if summary.failed > 0 || summary.retry_pending > 0 || summary.status == "interrupted" {
        println!("Resume: bookforge resume {}", args.job_id);
    } else if summary.needs_review > 0 {
        println!(
            "Review: segments need manual review; default resume skips needs-review segments."
        );
    }

    Ok(())
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
