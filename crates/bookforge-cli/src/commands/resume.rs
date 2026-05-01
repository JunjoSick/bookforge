use anyhow::Result;
use bookforge_store::JobStore;
use clap::Args;

#[derive(Debug, Args)]
pub struct ResumeArgs {
    pub job_id: String,
}

pub async fn run(args: ResumeArgs) -> Result<()> {
    let store = JobStore::open_default()?;
    let Some(summary) = store.summary(&args.job_id)? else {
        anyhow::bail!("job '{}' was not found", args.job_id);
    };

    println!("Job: {}", summary.id);
    println!("Status: {}", summary.status);
    println!("Segments: {}", summary.total_segments);
    println!("Succeeded: {}", summary.succeeded);
    println!("Failed: {}", summary.failed);
    println!("Needs review: {}", summary.needs_review);
    println!("Retry pending: {}", summary.retry_pending);
    println!("Input tokens: {}", summary.input_tokens);
    println!("Output tokens: {}", summary.output_tokens);

    if summary.failed == 0 && summary.needs_review == 0 && summary.retry_pending == 0 {
        println!("Resume: nothing to do; completed segments are already checkpointed.");
    } else {
        println!("Resume execution will be expanded once job plans are persisted.");
    }

    Ok(())
}
