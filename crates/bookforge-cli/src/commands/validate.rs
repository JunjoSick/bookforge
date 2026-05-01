use anyhow::Result;
use clap::Args;
use std::path::PathBuf;

#[derive(Debug, Args)]
pub struct ValidateArgs {
    pub input: PathBuf,
}

pub async fn run(args: ValidateArgs) -> Result<()> {
    println!("Input: {}", args.input.display());
    println!("Validation is not implemented yet.");
    Ok(())
}
