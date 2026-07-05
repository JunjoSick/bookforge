use anyhow::Result;
use bookforge_core::ControlCommand;
use clap::Args;

#[derive(Debug, Args)]
pub struct PauseArgs {
    pub job_id: String,
}

#[derive(Debug, Args)]
pub struct StopArgs {
    pub job_id: String,
}

pub async fn pause(args: PauseArgs) -> Result<()> {
    let path = crate::control::request_job_control(&args.job_id, ControlCommand::Pause)?;
    println!("pause requested for {} ({})", args.job_id, path.display());
    Ok(())
}

pub async fn stop(args: StopArgs) -> Result<()> {
    let path = crate::control::request_job_control(&args.job_id, ControlCommand::Stop)?;
    println!("stop requested for {} ({})", args.job_id, path.display());
    Ok(())
}
