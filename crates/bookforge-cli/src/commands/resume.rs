use anyhow::Result;
use clap::Args;

#[derive(Debug, Args)]
pub struct ResumeArgs {
    pub job_id: String,
}

pub async fn run(args: ResumeArgs) -> Result<()> {
    println!("Job: {}", args.job_id);
    println!("Resume is not implemented yet.");
    Ok(())
}
