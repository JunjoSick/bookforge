use anyhow::Result;
use bookforge_core::config::TranslationConfig;
use clap::Args;
use std::path::PathBuf;

use crate::{LanguageArgs, ProviderArgs, default_output_path};

#[derive(Debug, Args)]
pub struct TranslateArgs {
    pub input: PathBuf,

    #[command(flatten)]
    pub language: LanguageArgs,

    #[command(flatten)]
    pub provider: ProviderArgs,

    #[arg(long, default_value_t = 4)]
    pub concurrency: usize,

    #[arg(long)]
    pub out: Option<PathBuf>,
}

pub async fn run(args: TranslateArgs) -> Result<()> {
    let output = args
        .out
        .unwrap_or_else(|| default_output_path(&args.input, &args.language.target));
    let config = TranslationConfig {
        source_language: args.language.source,
        target_language: args.language.target,
        provider: args.provider.provider,
        model: args.provider.model,
        concurrency: args.concurrency,
        output,
    };

    println!("Input: {}", args.input.display());
    println!("Output: {}", config.output.display());
    println!("Target: {}", config.target_language);
    println!("Provider: {}", config.provider);
    println!("Concurrency: {}", config.concurrency);
    println!("Translation is not implemented yet.");

    Ok(())
}
