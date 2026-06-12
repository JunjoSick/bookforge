mod checkpoint;
mod commands;
mod cost;
mod performance;
mod progress;
mod report;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use commands::{
    doctor, entity, estimate, glossary, ingest_flags, inspect, resume, retry, review, status,
    style, tail, translate, validate,
};
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;
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
    Translate(Box<translate::TranslateArgs>),
    Resume(resume::ResumeArgs),
    Review(review::ReviewArgs),
    IngestFlags(ingest_flags::IngestFlagsArgs),
    Glossary(glossary::GlossaryArgs),
    Retry(retry::RetryArgs),
    Validate(validate::ValidateArgs),
    Benchmark(Box<translate::BenchmarkArgs>),
    Doctor(doctor::DoctorArgs),
    Entities(entity::EntitiesArgs),
    Status(status::StatusArgs),
    Style(style::StyleArgs),
    Tail(tail::TailArgs),
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    install_panic_hook();

    let cancel_token = CancellationToken::new();
    let cancel = cancel_token.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        cancel.cancel();
    });

    match Cli::parse().command {
        Command::Inspect(args) => inspect::run(args).await,
        Command::Estimate(args) => estimate::run(args).await,
        Command::Translate(args) => translate::run(*args, cancel_token).await,
        Command::Resume(args) => resume::run(args).await,
        Command::Review(args) => review::run(args).await,
        Command::IngestFlags(args) => ingest_flags::run(args).await,
        Command::Glossary(args) => glossary::run(args).await,
        Command::Retry(args) => retry::run(args).await,
        Command::Validate(args) => validate::run(args).await,
        Command::Benchmark(args) => translate::run_benchmark(*args).await,
        Command::Doctor(args) => doctor::run(args).await,
        Command::Entities(args) => entity::run(args).await,
        Command::Status(args) => status::run(args).await,
        Command::Style(args) => style::run(args).await,
        Command::Tail(args) => tail::run(args).await,
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}

/// Install a panic hook that attempts to restore terminal state before
/// printing the panic trace. Without this, indicatif can leave the terminal
/// in a broken state (hidden cursor, overwritten lines) on panic.
pub fn install_panic_hook() {
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        // Try to restore terminal visibility
        let _ = console::Term::stderr().show_cursor();
        eprintln!();
        previous_hook(panic_info);
    }));
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

    #[arg(long)]
    pub(crate) timeout_seconds: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum QaMode {
    Off,
    Suspicious,
    All,
}

fn default_output_path(input: &std::path::Path, target: &str) -> PathBuf {
    let stem = input
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("book");
    let target = target
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>();
    let target = target.trim_matches('-');
    input.with_file_name(format!("{stem}.{target}.epub"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookforge_core::config::TranslationProfile;

    #[test]
    fn translate_defaults_to_v1_fast_profile() {
        let cli = Cli::parse_from(["bookforge", "translate", "book.epub", "--target", "Italian"]);

        match cli.command {
            Command::Translate(args) => {
                assert_eq!(args.profile, TranslationProfile::V1Fast);
            }
            _ => panic!("expected translate command"),
        }
    }
}
