use anyhow::Result;
use clap::Args;
use std::path::PathBuf;

use crate::{LanguageArgs, ProviderArgs};

#[derive(Debug, Args)]
pub struct EstimateArgs {
    pub input: PathBuf,

    #[command(flatten)]
    pub language: LanguageArgs,

    #[command(flatten)]
    pub provider: ProviderArgs,
}

pub async fn run(args: EstimateArgs) -> Result<()> {
    println!("Input: {}", args.input.display());
    println!("Target: {}", args.language.target);
    println!("Provider: {}", args.provider.provider);
    println!("Estimate is not implemented yet.");
    Ok(())
}
