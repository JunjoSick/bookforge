use clap::Args;

use bookforge_store::JobStore;

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

    let event_log_path =
        std::path::PathBuf::from(format!(".bookforge/runs/{}/events.jsonl", args.job_id));
    if event_log_path.exists() {
        println!("Event log: {}", event_log_path.display());
    }
    let report_path =
        std::path::PathBuf::from(format!(".bookforge/runs/{}/report.md", args.job_id));
    if report_path.exists() {
        println!("Report: {}", report_path.display());
    }

    if matches!(
        summary.status.as_str(),
        "failed" | "interrupted" | "retry_pending"
    ) {
        println!("Resume: bookforge resume {}", args.job_id);
    }

    Ok(())
}
