use anyhow::Result;
use bookforge_store::{JobStore, RetryScope as StoreRetryScope};
use clap::{Args, ValueEnum};

#[derive(Debug, Args)]
pub struct RetryArgs {
    pub job_id: String,

    #[arg(long, value_enum, default_value_t = RetryScope::Failed)]
    pub only: RetryScope,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum RetryScope {
    Failed,
    NeedsReview,
    All,
}

pub async fn run(args: RetryArgs) -> Result<()> {
    let store = JobStore::open_default()?;
    if store.get_job(&args.job_id)?.is_none() {
        anyhow::bail!("job '{}' was not found", args.job_id);
    }

    let count = store.retry_segments(&args.job_id, args.only.into())?;
    println!("Job: {}", args.job_id);
    println!("Retry scope: {:?}", args.only);
    println!("Marked retry_pending: {count}");
    Ok(())
}

impl From<RetryScope> for StoreRetryScope {
    fn from(value: RetryScope) -> Self {
        match value {
            RetryScope::Failed => StoreRetryScope::Failed,
            RetryScope::NeedsReview => StoreRetryScope::NeedsReview,
            RetryScope::All => StoreRetryScope::All,
        }
    }
}
