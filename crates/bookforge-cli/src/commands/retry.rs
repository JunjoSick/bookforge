use anyhow::Result;
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
    println!("Job: {}", args.job_id);
    println!("Retry scope: {:?}", args.only);
    println!("Retry is not implemented yet.");
    Ok(())
}
