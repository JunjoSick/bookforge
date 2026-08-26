mod audio_cost;
mod checkpoint;
mod commands;
mod control;
mod cost;
mod epoch;
#[cfg(any(feature = "tui", feature = "serve"))]
mod eventlog;
mod exit_code;
mod performance;
mod progress;
mod report;
pub(crate) mod sanitize;
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
    audiobook, control as control_commands, convert, correct, doctor, entity, estimate, glossary,
    ingest_flags, inspect, plan, reconfigure, reflow, resume, retry, review, status, style, tail,
    translate, validate,
};
#[cfg(any(test, not(feature = "serve")))]
use std::io::Write;
use std::{
    fs,
    io::{self, ErrorKind},
    path::{Path, PathBuf},
    time::SystemTime,
};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Debug, Parser)]
#[command(
    name = "bookforge",
    version,
    about = "EPUB-first AI book translation tool",
    after_help = exit_codes_help_text()
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Convert a PDF into a translation-ready EPUB.
    Convert(convert::ConvertArgs),
    /// Repair paragraph flow in an EPUB without translating it.
    Reflow(reflow::ReflowArgs),
    /// Inspect EPUB structure and translatable text coverage.
    Inspect(inspect::InspectArgs),
    /// Generate resumable audiobook files from an EPUB.
    Audiobook(audiobook::AudiobookArgs),
    /// Estimate translation tokens and provider cost.
    Estimate(estimate::EstimateArgs),
    /// Inspect an EPUB offline and recommend translation settings.
    Plan(plan::PlanArgs),
    /// Translate an EPUB and checkpoint every completed segment.
    Translate(Box<translate::TranslateArgs>),
    /// Ask a running job to pause after its current safe boundary.
    Pause(control_commands::PauseArgs),
    /// Change cache-safe settings for an active or resumable job.
    Reconfigure(reconfigure::ReconfigureArgs),
    /// Continue an interrupted, paused, stopped, or incomplete job.
    Resume(resume::ResumeArgs),
    /// Ask a running job to stop after its current safe boundary.
    Stop(control_commands::StopArgs),
    /// Replace a checkpointed segment with a validated human correction.
    Correct(correct::CorrectArgs),
    /// Generate a side-by-side HTML review for a translation job.
    Review(review::ReviewArgs),
    /// Import flags exported from a review page.
    IngestFlags(ingest_flags::IngestFlagsArgs),
    /// Manage terminology and glossary candidates.
    Glossary(glossary::GlossaryArgs),
    /// Mark failed or review-needed segments for another attempt.
    Retry(retry::RetryArgs),
    /// Validate an EPUB and optionally run EPUBCheck.
    Validate(validate::ValidateArgs),
    /// Measure provider latency and throughput with synthetic requests.
    Benchmark(Box<translate::BenchmarkArgs>),
    /// Check storage, provider, PDF, and OCR dependencies.
    Doctor(doctor::DoctorArgs),
    /// Manage reusable named-entity guidance.
    Entities(entity::EntitiesArgs),
    /// Show persisted state, progress, and performance for a job.
    Status(status::StatusArgs),
    /// Manage reusable translation style guidance.
    Style(style::StyleArgs),
    /// Print recent events from a job's durable event log.
    Tail(tail::TailArgs),
    #[cfg(feature = "tui")]
    /// Monitor and control a job in a full-screen terminal UI.
    Watch(watch::WatchArgs),
    #[cfg(feature = "serve")]
    /// Run the local browser dashboard.
    Serve(serve::ServeArgs),
}

fn exit_codes_help_text() -> &'static str {
    #[cfg(feature = "serve")]
    {
        concat!(
            "Exit codes: 0 success or intentional stop · 1 runtime failure · ",
            "2 usage error · 3 job finished with failed/needs-review segments · ",
            "130 interrupted by Ctrl+C (progress saved)\n",
            "Run `bookforge` without a command to open the local browser dashboard."
        )
    }
    #[cfg(not(feature = "serve"))]
    {
        concat!(
            "Exit codes: 0 success or intentional stop · 1 runtime failure · ",
            "2 usage error · 3 job finished with failed/needs-review segments · ",
            "130 interrupted by Ctrl+C (progress saved)\n",
            "This build was compiled without the local browser dashboard."
        )
    }
}

#[tokio::main]
async fn main() {
    init_tracing();
    install_panic_hook();
    sweep_stale_retry_override_dirs_at_startup();

    let cancel_token = CancellationToken::new();
    let cancel = cancel_token.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        cancel.cancel();
    });

    let result = match parse_cli().command {
        Some(command) => run_command(command, cancel_token).await,
        None => run_default().await,
    };
    std::process::exit(match result {
        Ok(()) => exit_code::resolve(false),
        // Preserve the `Error: …` diagnostic that Rust's Termination impl used
        // to print for `async fn main() -> anyhow::Result<()>`.
        Err(err) => {
            eprintln!("Error: {err:?}");
            exit_code::resolve(true)
        }
    });
}

/// Parse the CLI. On a parse error the error is printed and the process exits
/// with clap's usage code (2), so this never returns `Err` to `main`.
fn parse_cli() -> Cli {
    match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            let exit_code = err.exit_code();
            if let Err(print_err) = err.print()
                && !is_broken_pipe(&print_err)
            {
                eprintln!("Error: {print_err}");
                std::process::exit(exit_code::FAILURE);
            }
            std::process::exit(exit_code);
        }
    }
}

async fn run_command(command: Command, cancel_token: CancellationToken) -> Result<()> {
    match command {
        Command::Convert(args) => convert::run(args).await,
        Command::Reflow(args) => reflow::run(args).await,
        Command::Inspect(args) => inspect::run(args).await,
        Command::Audiobook(args) => audiobook::run(args, cancel_token).await,
        Command::Estimate(args) => estimate::run(args).await,
        Command::Plan(args) => plan::run(args).await,
        Command::Translate(args) => translate::run(*args, cancel_token).await,
        Command::Pause(args) => control_commands::pause(args).await,
        Command::Reconfigure(args) => reconfigure::run(args).await,
        Command::Resume(args) => resume::run(args, cancel_token).await,
        Command::Stop(args) => control_commands::stop(args).await,
        Command::Correct(args) => correct::run(args).await,
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
        no_auth: false,
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

// ---------------------------------------------------------------------------
// INFRA-10: startup sweep for abandoned `retry_pending_overrides_<pid>` run
// directories under `.bookforge/runs`.
//
// These directories are created for retry-pending override sidecars; when the
// owner process dies between creating and clearing one, an empty directory
// lingers forever. A previous audit counted 51 of them. The sweep only ever
// deletes directories that are (a) named after a parseable pid, (b) provably
// empty, and (c) provably not owned by a live process — or, where liveness
// cannot be determined (Windows), older than RETRY_OVERRIDE_FALLBACK_MAX_AGE.
// Directories with any content are never touched, because they may hold a
// live worker's pending override sidecar.
// ---------------------------------------------------------------------------

const RETRY_OVERRIDE_DIR_PREFIX: &str = "retry_pending_overrides_";
const RETRY_OVERRIDES_RUNS_ROOT: &str = ".bookforge/runs";
/// Windows-safe fallback window (INFRA-10): without a portable liveness probe,
/// only treat a dir as stale once it has sat untouched for at least this long.
const RETRY_OVERRIDE_FALLBACK_MAX_AGE: std::time::Duration =
    std::time::Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OwnerLiveness {
    Alive,
    Gone,
    /// Platform cannot tell cheaply and safely (e.g. Windows fallback) — or
    /// the probe itself failed on a non-Linux Unix box.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    Indeterminate,
}

fn sweep_stale_retry_override_dirs_at_startup() {
    let reaped = sweep_stale_retry_override_dirs(Path::new(RETRY_OVERRIDES_RUNS_ROOT));
    if reaped > 0 {
        tracing::info!("reaped {reaped} empty retry_pending_overrides directories");
    }
}

fn sweep_stale_retry_override_dirs(runs_root: &Path) -> usize {
    let Ok(entries) = fs::read_dir(runs_root) else {
        // No runs root (or unreadable): nothing to sweep, never fail startup.
        return 0;
    };

    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_text) = name.to_str() else {
            continue;
        };
        let Some(pid_text) = name_text.strip_prefix(RETRY_OVERRIDE_DIR_PREFIX) else {
            continue;
        };
        let Ok(pid) = pid_text.parse::<u32>() else {
            continue;
        };
        if !entry
            .file_type()
            .map(|file_type| file_type.is_dir())
            .unwrap_or(false)
        {
            continue;
        }
        let is_empty = fs::read_dir(entry.path())
            .map(|mut contents| contents.next().is_none())
            .unwrap_or(false);
        if !is_empty {
            continue;
        }

        let stale_since_fallback_window =
            dir_idle_for_at_least(&entry.path(), RETRY_OVERRIDE_FALLBACK_MAX_AGE);
        if retry_override_dir_is_reapable(owner_liveness(pid), stale_since_fallback_window)
            && fs::remove_dir(entry.path()).is_ok()
        {
            removed += 1;
        }
    }
    removed
}

fn retry_override_dir_is_reapable(
    liveness: OwnerLiveness,
    idle_over_fallback_window: bool,
) -> bool {
    match liveness {
        OwnerLiveness::Alive => false,
        OwnerLiveness::Gone => true,
        OwnerLiveness::Indeterminate => idle_over_fallback_window,
    }
}

fn dir_idle_for_at_least(path: &Path, min_age: std::time::Duration) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age >= min_age)
}

/// Liveness of the pid encoded in a `retry_pending_overrides_<pid>` name.
///
/// Fail-closed like the SERVE-3 signal path (`serve/audio.rs`): when identity
/// cannot be established we return [`OwnerLiveness::Indeterminate`] instead of
/// guessing, and the caller falls back to the conservative age rule.
fn owner_liveness(pid: u32) -> OwnerLiveness {
    if pid == std::process::id() {
        return OwnerLiveness::Alive;
    }

    #[cfg(target_os = "linux")]
    {
        if PathBuf::from(format!("/proc/{pid}")).exists() {
            OwnerLiveness::Alive
        } else {
            OwnerLiveness::Gone
        }
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    {
        match std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "comm="])
            .output()
        {
            Ok(output) if output.status.success() && !output.stdout.trim().is_empty() => {
                OwnerLiveness::Alive
            }
            Ok(_) => OwnerLiveness::Gone,
            Err(_) => OwnerLiveness::Indeterminate,
        }
    }

    #[cfg(not(unix))]
    {
        OwnerLiveness::Indeterminate
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum QaMode {
    Off,
    Suspicious,
    All,
}

impl QaMode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Suspicious => "suspicious",
            Self::All => "all",
        }
    }

    pub(crate) fn from_snapshot(value: &str) -> Self {
        match value {
            "suspicious" => Self::Suspicious,
            "all" => Self::All,
            _ => Self::Off,
        }
    }
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
mod sweep_tests {
    use super::*;
    use std::{
        fs,
        path::PathBuf,
        time::{Duration, SystemTime},
    };

    fn temp_runs_root(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "bookforge-sweep-{label}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("runs root creates");
        dir
    }

    #[test]
    fn reap_decision_never_touches_live_owners() {
        use OwnerLiveness::{Alive, Gone, Indeterminate};
        assert!(!retry_override_dir_is_reapable(Alive, false));
        assert!(!retry_override_dir_is_reapable(Alive, true));
        assert!(retry_override_dir_is_reapable(Gone, false));
        assert!(retry_override_dir_is_reapable(Gone, true));
        // Indeterminate platforms (Windows) demand the >=24h age window.
        assert!(!retry_override_dir_is_reapable(Indeterminate, false));
        assert!(retry_override_dir_is_reapable(Indeterminate, true));
    }

    #[test]
    fn missing_runs_root_is_a_no_op() {
        assert_eq!(
            sweep_stale_retry_override_dirs(Path::new("/nonexistent/bookforge-runs")),
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn empty_dir_of_dead_owner_is_swept_alive_owner_and_content_survive() {
        let root = temp_runs_root("unix");
        // A pid beyond any plausible pid_max on linux/BSDs/macOS.
        let dead_pid = u32::MAX - 1;
        let dead_empty = root.join(format!("{RETRY_OVERRIDE_DIR_PREFIX}{dead_pid}"));
        fs::create_dir(&dead_empty).expect("empty dead-owner dir");
        let live_dir = root.join(format!("{RETRY_OVERRIDE_DIR_PREFIX}{}", std::process::id()));
        fs::create_dir(&live_dir).expect("self-owned (alive) dir");
        let busy_dead = root.join(format!("{RETRY_OVERRIDE_DIR_PREFIX}{}", u32::MAX - 2));
        fs::create_dir(&busy_dead).expect("dead-owner with content");
        fs::write(busy_dead.join("overrides.json"), "{}").expect("sidecar content");
        fs::create_dir(root.join("something_else")).expect("unrelated dir");

        let removed = sweep_stale_retry_override_dirs(&root);
        assert_eq!(removed, 1, "exactly the empty dead-owner dir is swept");
        assert!(!dead_empty.exists());
        assert!(live_dir.exists(), "dirs of live pids are NEVER deleted");
        assert!(busy_dead.exists(), "non-empty dirs are never deleted");
        assert!(root.join("something_else").exists());

        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn malformed_names_are_ignored_entirely() {
        let root = temp_runs_root("junk");
        for junk in [
            format!("{RETRY_OVERRIDE_DIR_PREFIX}notanumber"),
            format!("{RETRY_OVERRIDE_DIR_PREFIX}-1"),
            RETRY_OVERRIDE_DIR_PREFIX.to_string(),
        ] {
            fs::create_dir(root.join(junk)).expect("junk dir");
        }
        assert_eq!(
            sweep_stale_retry_override_dirs(&root),
            0,
            "no parseable pid means no sweep"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fallback_age_window_boundary_logic_is_exclusive_to_old_dirs() {
        let root = temp_runs_root("age");
        let dir = root.join(format!("{RETRY_OVERRIDE_DIR_PREFIX}{}", u32::MAX - 3));
        fs::create_dir(&dir).expect("fresh dir");
        // Fresh mtime: age is ~0, so any positive window rejects it...
        assert!(!dir_idle_for_at_least(
            &dir,
            Duration::from_secs(24 * 60 * 60)
        ));
        // ...while a zero window accepts it (proves the comparison runs).
        assert!(dir_idle_for_at_least(&dir, Duration::ZERO));
        let _ = fs::remove_dir_all(root);
    }
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
    fn reflow_requires_output_and_accepts_dry_run() {
        let cli = Cli::parse_from([
            "bookforge",
            "reflow",
            "source.epub",
            "--output",
            "reflowed.epub",
            "--dry-run",
            "--aggressive",
        ]);

        match cli.command {
            Some(Command::Reflow(args)) => {
                assert_eq!(args.input, PathBuf::from("source.epub"));
                assert_eq!(args.output, PathBuf::from("reflowed.epub"));
                assert!(args.dry_run);
                assert!(args.aggressive);
            }
            _ => panic!("expected reflow command"),
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

    #[test]
    fn every_top_level_command_has_help_text() {
        let command = Cli::command();
        let missing = command
            .get_subcommands()
            .filter(|subcommand| subcommand.get_about().is_none())
            .map(|subcommand| subcommand.get_name().to_string())
            .collect::<Vec<_>>();

        assert!(
            missing.is_empty(),
            "top-level commands without help text: {}",
            missing.join(", ")
        );
    }

    #[test]
    fn top_level_help_documents_the_exit_code_taxonomy() {
        let mut help = Vec::new();
        Cli::command()
            .write_long_help(&mut help)
            .expect("help should render");
        let help = String::from_utf8(help).expect("help should be utf-8");

        assert!(help.contains("Exit codes"));
        // Each documented bucket is named with its number.
        for needle in [
            "0 success",
            "1 runtime failure",
            "2 usage error",
            "3 job finished",
            "130 interrupted",
        ] {
            assert!(help.contains(needle), "help must mention `{needle}`");
        }
    }

    /// UI-13: tri-state bool flags accept the bare form (flag alone = true)
    /// and an explicit value (`=false` / ` false`) — the same syntax
    /// `reconfigure` uses, instead of translate-only "value required".
    #[test]
    fn tri_state_flags_accept_bare_and_explicit_forms() {
        let bare = Cli::parse_from([
            "bookforge",
            "translate",
            "book.epub",
            "--target",
            "Italian",
            "--compact-prompts",
            "--retry-failed-only",
            "--adaptive-concurrency",
        ]);
        match bare.command {
            Some(Command::Translate(args)) => {
                assert_eq!(args.compact_prompts, Some(true));
                assert_eq!(args.retry_failed_only, Some(true));
                assert_eq!(args.adaptive_concurrency, Some(true));
            }
            _ => panic!("expected translate command"),
        }

        let explicit = Cli::parse_from([
            "bookforge",
            "translate",
            "book.epub",
            "--target",
            "Italian",
            "--compact-prompts=false",
            "--retry-failed-only=false",
            "--adaptive-concurrency=false",
        ]);
        match explicit.command {
            Some(Command::Translate(args)) => {
                assert_eq!(args.compact_prompts, Some(false));
                assert_eq!(args.retry_failed_only, Some(false));
                assert_eq!(args.adaptive_concurrency, Some(false));
            }
            _ => panic!("expected translate command"),
        }
    }
}
