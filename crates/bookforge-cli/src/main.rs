mod commands;
mod report;

use anyhow::Result;
use clap::{Parser, Subcommand};
use commands::{estimate, inspect, resume, retry, translate, validate};
use std::path::PathBuf;
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Debug, Parser)]
#[command(
    name = "bookforge",
    version,
    about = "EPUB-first AI book translation tool"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Inspect(inspect::InspectArgs),
    Estimate(estimate::EstimateArgs),
    Translate(translate::TranslateArgs),
    Resume(resume::ResumeArgs),
    Retry(retry::RetryArgs),
    Validate(validate::ValidateArgs),
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    match Cli::parse().command {
        Command::Inspect(args) => inspect::run(args).await,
        Command::Estimate(args) => estimate::run(args).await,
        Command::Translate(args) => translate::run(args).await,
        Command::Resume(args) => resume::run(args).await,
        Command::Retry(args) => retry::run(args).await,
        Command::Validate(args) => validate::run(args).await,
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    fmt().with_env_filter(filter).with_target(false).init();
}

#[derive(Debug, Clone, clap::Args)]
struct LanguageArgs {
    #[arg(long)]
    source: Option<String>,

    #[arg(long)]
    target: String,
}

#[derive(Debug, Clone, clap::Args)]
pub(crate) struct ProviderArgs {
    #[arg(long, default_value = "deepseek")]
    pub(crate) provider: String,

    #[arg(long)]
    pub(crate) model: Option<String>,

    #[arg(long)]
    pub(crate) base_url: Option<String>,

    #[arg(long)]
    pub(crate) api_key_env: Option<String>,
}

fn default_output_path(input: &std::path::Path, target: &str) -> PathBuf {
    let stem = input
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("book");
    let target = target.to_lowercase().replace(' ', "-");
    input.with_file_name(format!("{stem}.{target}.epub"))
}
