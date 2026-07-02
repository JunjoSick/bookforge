mod checkpoint;
mod commands;
mod cost;
#[cfg(any(feature = "tui", feature = "serve"))]
mod eventlog;
mod performance;
mod progress;
mod report;
#[cfg(feature = "tui")]
mod tui;

use anyhow::Result;
#[cfg(any(test, not(feature = "serve")))]
use clap::CommandFactory;
use clap::{Parser, Subcommand, ValueEnum};
#[cfg(feature = "serve")]
use commands::serve;
#[cfg(feature = "tui")]
use commands::watch;
use commands::{
    convert, doctor, entity, estimate, glossary, ingest_flags, inspect, resume, retry, review,
    status, style, tail, translate, validate,
};
#[cfg(any(test, not(feature = "serve")))]
use std::io::Write;
use std::{
    io::{self, ErrorKind},
    path::PathBuf,
};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Debug, Parser)]
#[command(
    name = "bookforge",
    version,
    about = "EPUB-first AI book translation tool"
)]
#[cfg_attr(
    feature = "serve",
    command(after_help = "Run `bookforge` without a command to open the local browser dashboard.")
)]
#[cfg_attr(
    not(feature = "serve"),
    command(after_help = "This build was compiled without the local browser dashboard.")
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Convert(convert::ConvertArgs),
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
    #[cfg(feature = "tui")]
    Watch(watch::WatchArgs),
    #[cfg(feature = "serve")]
    Serve(serve::ServeArgs),
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

    match parse_cli()?.command {
        Some(command) => run_command(command, cancel_token).await,
        None => run_default().await,
    }
}

fn parse_cli() -> Result<Cli> {
    match Cli::try_parse() {
        Ok(cli) => Ok(cli),
        Err(err) => {
            let exit_code = err.exit_code();
            if let Err(print_err) = err.print()
                && !is_broken_pipe(&print_err)
            {
                return Err(print_err.into());
            }
            std::process::exit(exit_code);
        }
    }
}

async fn run_command(command: Command, cancel_token: CancellationToken) -> Result<()> {
    match command {
        Command::Convert(args) => convert::run(args).await,
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
        #[cfg(feature = "tui")]
        Command::Watch(args) => watch::run(args).await,
        #[cfg(feature = "serve")]
        Command::Serve(args) => serve::run(args).await,
    }
}

#[cfg(feature = "serve")]
async fn run_default() -> Result<()> {
    serve::run(serve::ServeArgs {
        bind: "127.0.0.1:8765".to_string(),
        open: true,
        refresh_ms: 250,
    })
    .await
}

#[cfg(not(feature = "serve"))]
async fn run_default() -> Result<()> {
    match write_default_help(io::stdout().lock()) {
        Ok(()) => Ok(()),
        Err(err) if is_broken_pipe(&err) => Ok(()),
        Err(err) => Err(err.into()),
    }
}

#[cfg(any(test, not(feature = "serve")))]
fn write_default_help(mut writer: impl Write) -> io::Result<()> {
    Cli::command().write_help(&mut writer)?;
    writeln!(writer)?;
    Ok(())
}

fn is_broken_pipe(err: &io::Error) -> bool {
    err.kind() == ErrorKind::BrokenPipe
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
            Some(Command::Translate(args)) => {
                assert_eq!(args.profile, TranslationProfile::V1Fast);
            }
            _ => panic!("expected translate command"),
        }
    }

    #[test]
    fn no_subcommand_selects_default_dashboard_entrypoint() {
        let cli = Cli::parse_from(["bookforge"]);

        assert!(cli.command.is_none());
    }

    #[test]
    fn broken_pipe_errors_are_recognized() {
        let err = io::Error::new(ErrorKind::BrokenPipe, "closed pipe");
        assert!(is_broken_pipe(&err));
    }

    #[test]
    fn default_help_writer_propagates_broken_pipe_for_caller_to_ignore() {
        struct BrokenPipeWriter;

        impl Write for BrokenPipeWriter {
            fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(ErrorKind::BrokenPipe, "closed pipe"))
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let err = write_default_help(BrokenPipeWriter)
            .expect_err("broken pipe should be returned to run_default");
        assert!(is_broken_pipe(&err));
    }

    #[cfg(feature = "serve")]
    #[test]
    fn help_mentions_default_dashboard_entrypoint() {
        let mut help = Vec::new();
        Cli::command()
            .write_long_help(&mut help)
            .expect("help should render");
        let help = String::from_utf8(help).expect("help should be utf-8");

        assert!(help.contains("Run `bookforge` without a command"));
    }
}
