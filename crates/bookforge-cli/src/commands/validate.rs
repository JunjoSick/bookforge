use std::{
    env, fs,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use bookforge_core::{config::SegmentationConfig, segment::build_segments};
use bookforge_epub::{ValidationSeverity, inspect_epub, read_epub, validate_translated_epub};
use clap::Args;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::output;

const VALIDATION_REPORT_SCHEMA_VERSION: u32 = 3;

/// Finite deadlines for EPUBCheck subprocesses. A hung `--version` probe or a
/// validation run that never exits must never block the CLI forever, and the
/// child must be killed and reaped on expiry rather than abandoned.
const EPUBCHECK_VERSION_DEADLINE: Duration = Duration::from_secs(30);
const EPUBCHECK_VALIDATION_DEADLINE: Duration = Duration::from_secs(600);
/// Per-stream retained output cap. stdout/stderr are drained concurrently but
/// only this much of each stream is ever buffered, so a chatty EPUBCheck
/// cannot grow process memory without bound.
const EPUBCHECK_STREAM_CAP_BYTES: usize = 128 * 1024;
const EPUBCHECK_POLL_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Debug, Args)]
#[command(
    after_help = "Environment:\n  BOOKFORGE_EPUBCHECK  Path to an EPUBCheck executable, wrapper script, or .jar (otherwise found on PATH)."
)]
pub struct ValidateArgs {
    pub input: PathBuf,

    /// JSON validation report path. Defaults to `<input>.validation.json`.
    #[arg(long)]
    pub report: Option<PathBuf>,

    /// Treat EPUBCheck warnings as validation failures.
    #[arg(long)]
    pub strict_epubcheck: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ValidationReport {
    schema_version: u32,
    epub_path: String,
    epubcheck: EpubCheckReport,
    bookforge_validators: BookforgeValidatorReport,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EpubCheckReport {
    ran: bool,
    version: Option<String>,
    status: String,
    messages: Vec<ValidationMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BookforgeValidatorReport {
    status: String,
    xml_valid: bool,
    package_path: Option<String>,
    spine_count: Option<usize>,
    xhtml_spine_count: Option<usize>,
    section_count: Option<usize>,
    block_count: Option<usize>,
    /// Diagnostic re-segmentation of the EPUB being validated. This is not a
    /// persisted scheduler count from the translation job.
    #[serde(alias = "segment_count")]
    default_segmentation_count: Option<usize>,
    #[serde(default = "schema_2_default_segmentation_max_tokens")]
    default_segmentation_max_tokens: usize,
    #[serde(default = "schema_2_default_segmentation_context_tokens")]
    default_segmentation_context_tokens: usize,
    estimated_token_count: Option<usize>,
    files_checked: usize,
    messages: Vec<ValidationMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ValidationMessage {
    severity: String,
    code: String,
    location: Option<String>,
    text: String,
}

#[derive(Debug)]
pub(crate) struct ValidationOutcome {
    pub report: ValidationReport,
    pub failed: bool,
}

pub async fn run(args: ValidateArgs) -> Result<()> {
    let report_path = args
        .report
        .unwrap_or_else(|| default_report_path(&args.input));
    let outcome = validate_and_write(&args.input, &report_path, args.strict_epubcheck)?;

    println!("Input: {}", args.input.display());
    println!(
        "BookForge validators: {}",
        outcome.report.bookforge_validators.status
    );
    println!("EPUBCheck: {}", outcome.report.epubcheck.status);
    if outcome.report.epubcheck.status == "unavailable" {
        eprintln!(
            "warning: EPUBCheck is unavailable; set BOOKFORGE_EPUBCHECK or install epubcheck on PATH"
        );
    }
    println!("Report: {}", report_path.display());

    if outcome.failed {
        bail!("EPUB validation failed; see {}", report_path.display());
    }
    println!("Validation: ok");
    Ok(())
}

pub(crate) fn validate_and_write(
    input: &Path,
    report_path: &Path,
    strict_epubcheck: bool,
) -> Result<ValidationOutcome> {
    output::ensure_distinct_paths("EPUB input/report", input, report_path)?;
    let outcome = validate_path(input, strict_epubcheck);
    output::write_atomic(
        report_path,
        serde_json::to_string_pretty(&outcome.report)?.as_bytes(),
    )
    .with_context(|| format!("writing validation report {}", report_path.display()))?;
    Ok(outcome)
}

pub(crate) fn default_report_path(input: &Path) -> PathBuf {
    let stem = input
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("book");
    input.with_file_name(format!("{stem}.validation.json"))
}

pub(crate) fn validate_path(input: &Path, strict_epubcheck: bool) -> ValidationOutcome {
    let bookforge_validators = run_bookforge_validators(input);
    let epubcheck = run_epubcheck(input);
    let failed = validation_failed(
        &bookforge_validators.status,
        &epubcheck.status,
        strict_epubcheck,
    );

    ValidationOutcome {
        report: ValidationReport {
            schema_version: VALIDATION_REPORT_SCHEMA_VERSION,
            epub_path: input.display().to_string(),
            epubcheck,
            bookforge_validators,
        },
        failed,
    }
}

fn validation_failed(bookforge_status: &str, epubcheck_status: &str, strict: bool) -> bool {
    bookforge_status == "errors"
        || epubcheck_status == "errors"
        || (strict && epubcheck_status == "warnings")
}

fn run_bookforge_validators(input: &Path) -> BookforgeValidatorReport {
    let structural = validate_translated_epub(input, &[], &[]);
    let mut messages = structural
        .issues
        .iter()
        .map(|issue| ValidationMessage {
            severity: severity_label(issue.severity).to_string(),
            code: issue.kind.clone(),
            location: issue.href.clone(),
            text: issue.message.clone(),
        })
        .collect::<Vec<_>>();

    let inspection = inspect_epub(input);
    let book = read_epub(input);
    let segmentation = SegmentationConfig::default();
    let mut default_segmentation_count = None;
    let mut estimated_token_count = None;
    let mut section_count = None;
    let mut block_count = None;

    if let Err(error) = &inspection {
        messages.push(ValidationMessage {
            severity: "error".to_string(),
            code: "inspection_failed".to_string(),
            location: None,
            text: error.to_string(),
        });
    }

    if let Ok(book) = &book {
        section_count = Some(book.sections.len());
        block_count = Some(book.blocks.len());
        match build_segments(book, &segmentation) {
            Ok(segments) => {
                default_segmentation_count = Some(segments.len());
                estimated_token_count = Some(
                    segments
                        .iter()
                        .map(|segment| segment.source.token_estimate)
                        .sum(),
                );
            }
            Err(error) => messages.push(ValidationMessage {
                severity: "error".to_string(),
                code: "segmentation_failed".to_string(),
                location: None,
                text: error.to_string(),
            }),
        }
    } else if let Err(error) = &book {
        messages.push(ValidationMessage {
            severity: "error".to_string(),
            code: "read_failed".to_string(),
            location: None,
            text: error.to_string(),
        });
    }

    let status = status_from_messages(&messages);
    BookforgeValidatorReport {
        status,
        xml_valid: structural.xml_valid,
        package_path: inspection
            .as_ref()
            .ok()
            .map(|value| value.package_path.clone()),
        spine_count: inspection.as_ref().ok().map(|value| value.spine_count),
        xhtml_spine_count: inspection
            .as_ref()
            .ok()
            .map(|value| value.xhtml_spine_count),
        section_count,
        block_count,
        default_segmentation_count,
        default_segmentation_max_tokens: segmentation.max_segment_tokens,
        default_segmentation_context_tokens: segmentation.context_tokens,
        estimated_token_count,
        files_checked: structural.files_checked,
        messages,
    }
}

fn schema_2_default_segmentation_max_tokens() -> usize {
    1_200
}

fn schema_2_default_segmentation_context_tokens() -> usize {
    160
}

fn severity_label(severity: ValidationSeverity) -> &'static str {
    match severity {
        ValidationSeverity::Info => "info",
        ValidationSeverity::Warning => "warning",
        ValidationSeverity::Error => "error",
    }
}

fn status_from_messages(messages: &[ValidationMessage]) -> String {
    if messages
        .iter()
        .any(|message| matches!(message.severity.as_str(), "fatal" | "error"))
    {
        "errors".to_string()
    } else if messages.iter().any(|message| message.severity == "warning") {
        "warnings".to_string()
    } else {
        "valid".to_string()
    }
}

#[derive(Debug, Clone)]
enum EpubCheckCommand {
    Direct(PathBuf),
    JavaJar { java: PathBuf, jar: PathBuf },
    WindowsScript(PathBuf),
}

impl EpubCheckCommand {
    /// Base process for this invocation: the resolved command line plus a
    /// scrubbed child environment. Provider/API credentials in the parent
    /// environment are never inherited by EPUBCheck, Java, or wrapper
    /// scripts; only the allowlisted variables needed for them to run are
    /// carried over.
    fn command(&self) -> Command {
        let mut command = match self {
            EpubCheckCommand::Direct(path) => Command::new(path),
            EpubCheckCommand::JavaJar { java, jar } => {
                let mut command = Command::new(java);
                command.arg("-jar").arg(jar);
                command
            }
            EpubCheckCommand::WindowsScript(path) => {
                let mut command = Command::new("cmd");
                command.arg("/C").arg(path);
                command
            }
        };
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;

            // Run in an isolated process group so a deadline kill can
            // terminate a wrapper (shell script / cmd) and its descendants
            // (java) together instead of abandoning a runaway grandchild.
            let _ = command.process_group(0);
        }
        apply_epubcheck_environment_allowlist(&mut command, &mut |name| env::var_os(name));
        command
    }

    /// Run `args` (plus `input` when present) under a finite `deadline`:
    /// stdin is nulled, stdout/stderr are piped and drained concurrently
    /// with bounded retained memory, and on expiry the process tree is
    /// killed and reaped (`EpubcheckCapture::timed_out`).
    fn run(
        &self,
        args: &[&str],
        input: Option<&Path>,
        deadline: Duration,
    ) -> io::Result<EpubcheckCapture> {
        let mut command = self.command();
        command.args(args);
        if let Some(input) = input {
            command.arg(input);
        }
        run_epubcheck_bounded(command, deadline, EPUBCHECK_STREAM_CAP_BYTES)
    }
}

/// Bounded output of one EPUBCheck subprocess run.
#[derive(Debug)]
struct EpubcheckCapture {
    status: Option<ExitStatus>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_exceeded: bool,
    stderr_exceeded: bool,
    timed_out: bool,
}

/// Spawn `command`, drain both output streams concurrently while retaining
/// at most `stream_cap` bytes each, and reap the child when it exits or when
/// `deadline` expires (killing the process group first). stdin is null so a
/// tool waiting on a tty fails fast instead of hanging forever.
fn run_epubcheck_bounded(
    mut command: Command,
    deadline: Duration,
    stream_cap: usize,
) -> io::Result<EpubcheckCapture> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("EPUBCheck stdout was not piped"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("EPUBCheck stderr was not piped"))?;
    let stdout_reader = match spawn_epubcheck_stream_reader(stdout, stream_cap, "stdout") {
        Ok(reader) => reader,
        Err(error) => {
            terminate_epubcheck_child(&mut child);
            return Err(error);
        }
    };
    let stderr_reader = match spawn_epubcheck_stream_reader(stderr, stream_cap, "stderr") {
        Ok(reader) => reader,
        Err(error) => {
            terminate_epubcheck_child(&mut child);
            let _ = stdout_reader.join();
            return Err(error);
        }
    };

    let started = Instant::now();
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if started.elapsed() >= deadline {
            timed_out = true;
            terminate_epubcheck_child(&mut child);
            break None;
        }
        let delay = deadline
            .saturating_sub(started.elapsed())
            .min(EPUBCHECK_POLL_INTERVAL);
        if delay.is_zero() {
            timed_out = true;
            terminate_epubcheck_child(&mut child);
            break None;
        }
        thread::sleep(delay);
    };

    let stdout = join_epubcheck_stream_reader(stdout_reader)?;
    let stderr = join_epubcheck_stream_reader(stderr_reader)?;
    Ok(EpubcheckCapture {
        status,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
        stdout_exceeded: stdout.exceeded,
        stderr_exceeded: stderr.exceeded,
        timed_out,
    })
}

struct EpubcheckStreamCapture {
    bytes: Vec<u8>,
    exceeded: bool,
}

fn spawn_epubcheck_stream_reader<R>(
    mut reader: R,
    cap: usize,
    stream: &'static str,
) -> io::Result<thread::JoinHandle<io::Result<EpubcheckStreamCapture>>>
where
    R: Read + Send + 'static,
{
    thread::Builder::new()
        .name(format!("epubcheck-{stream}-reader"))
        .spawn(move || {
            let mut bytes = Vec::with_capacity(cap.min(64 * 1024));
            let mut exceeded = false;
            let mut buffer = [0_u8; 16 * 1024];
            loop {
                let count = reader.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                let retained = count.min(cap.saturating_sub(bytes.len()));
                bytes.extend_from_slice(&buffer[..retained]);
                exceeded |= retained < count;
            }
            Ok(EpubcheckStreamCapture { bytes, exceeded })
        })
}

fn join_epubcheck_stream_reader(
    reader: thread::JoinHandle<io::Result<EpubcheckStreamCapture>>,
) -> io::Result<EpubcheckStreamCapture> {
    reader
        .join()
        .map_err(|_| io::Error::other("EPUBCheck output reader thread panicked"))?
}

/// Kill the child process (group) and reap it. On Unix the child was placed
/// in its own process group before spawn, so the negative-PID signal covers
/// wrapper + java descendants; the direct kill + wait is the cross-platform
/// fallback and reaps the process so no zombie is left behind.
fn terminate_epubcheck_child(child: &mut Child) {
    #[cfg(unix)]
    {
        let pid = child.id();
        if pid != 0 {
            let _ = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Minimal cross-platform environment allowlist for EPUBCheck child
/// processes. Starting from `env_clear` and re-adding only these names means
/// provider/API credentials and other secrets are never inherited, while Java
/// (`JAVA_HOME`, temp dirs), wrapper scripts (`PATH`, `HOME`, locale) and
/// Windows `cmd` (`SYSTEMROOT`/`COMSPEC`) keep working.
fn apply_epubcheck_environment_allowlist(
    command: &mut Command,
    lookup: &mut dyn FnMut(&str) -> Option<std::ffi::OsString>,
) {
    command.env_clear();
    for name in epubcheck_environment_allowlist() {
        if let Some(value) = lookup(name) {
            command.env(name, value);
        }
    }
}

fn epubcheck_environment_allowlist() -> &'static [&'static str] {
    #[cfg(windows)]
    {
        &[
            "PATH",
            "HOME",
            "JAVA_HOME",
            "TMPDIR",
            "TMP",
            "TEMP",
            "LANG",
            "LC_ALL",
            "LC_CTYPE",
            "LANGUAGE",
            "TZ",
            "SYSTEMROOT",
            "WINDIR",
            "COMSPEC",
            "PATHEXT",
            "USERPROFILE",
            "HOMEDRIVE",
            "HOMEPATH",
            "NUMBER_OF_PROCESSORS",
            "PROCESSOR_ARCHITECTURE",
        ]
    }
    #[cfg(not(windows))]
    {
        &[
            "PATH",
            "HOME",
            "JAVA_HOME",
            "TMPDIR",
            "TMP",
            "TEMP",
            "LANG",
            "LC_ALL",
            "LC_CTYPE",
            "LANGUAGE",
            "TZ",
        ]
    }
}

fn run_epubcheck(input: &Path) -> EpubCheckReport {
    let command = match discover_epubcheck() {
        Ok(command) => command,
        Err(message) => return unavailable_epubcheck(message),
    };

    let version = command
        .run(&["--version"], None, EPUBCHECK_VERSION_DEADLINE)
        .ok()
        .filter(|capture| !capture.timed_out)
        .and_then(|capture| {
            let text = format!(
                "{}\n{}",
                String::from_utf8_lossy(&capture.stdout),
                String::from_utf8_lossy(&capture.stderr)
            );
            parse_version_banner(&text)
        });

    let report_path = env::temp_dir().join(format!(
        "bookforge-epubcheck-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let report_arg = report_path.to_string_lossy().into_owned();
    let capture = match command.run(
        &["--json", &report_arg],
        Some(input),
        EPUBCHECK_VALIDATION_DEADLINE,
    ) {
        Ok(capture) => capture,
        Err(error) => {
            // Spawn failures happen before any child runs; remove the (never
            // created) temp path anyway so no stale report can survive.
            let _ = fs::remove_file(&report_path);
            return unavailable_epubcheck(format!("failed to run EPUBCheck: {error}"));
        }
    };
    let stderr = String::from_utf8_lossy(&capture.stderr);
    if capture.stdout_exceeded {
        tracing::warn!(
            "EPUBCheck exceeded the {}-byte stdout capture limit; output is truncated",
            EPUBCHECK_STREAM_CAP_BYTES
        );
    }
    if capture.stderr_exceeded {
        tracing::warn!(
            "EPUBCheck exceeded the {}-byte stderr capture limit; diagnostics are truncated",
            EPUBCHECK_STREAM_CAP_BYTES
        );
    }
    if capture.timed_out {
        let _ = fs::remove_file(&report_path);
        return EpubCheckReport {
            ran: true,
            version,
            status: "errors".to_string(),
            messages: vec![ValidationMessage {
                severity: "error".to_string(),
                code: "EPUBCHECK-TIMEOUT".to_string(),
                location: None,
                text: format!(
                    "EPUBCheck did not finish within {} seconds and was terminated. stderr: {}",
                    EPUBCHECK_VALIDATION_DEADLINE.as_secs(),
                    stderr.trim()
                ),
            }],
        };
    }
    let report_json = fs::read_to_string(&report_path);
    // The report file is private temp state for one validation; remove it as
    // soon as its bytes are no longer needed, on every path below.
    let _ = fs::remove_file(&report_path);
    let report_json = match report_json {
        Ok(report) => report,
        Err(error) => {
            return EpubCheckReport {
                ran: true,
                version,
                status: "errors".to_string(),
                messages: vec![ValidationMessage {
                    severity: "error".to_string(),
                    code: "EPUBCHECK-REPORT".to_string(),
                    location: None,
                    text: format!(
                        "EPUBCheck did not write its JSON report: {error}. stderr: {}",
                        stderr.trim()
                    ),
                }],
            };
        }
    };
    match parse_epubcheck_json(&report_json, version.clone()) {
        Ok(mut report) => {
            if report.messages.is_empty() && !capture.status.is_some_and(|status| status.success())
            {
                report.messages.push(ValidationMessage {
                    severity: "error".to_string(),
                    code: "EPUBCHECK-EXIT".to_string(),
                    location: None,
                    text: stderr.trim().to_string(),
                });
                report.status = "errors".to_string();
            }
            report
        }
        Err(error) => EpubCheckReport {
            ran: true,
            version,
            status: "errors".to_string(),
            messages: vec![ValidationMessage {
                severity: "error".to_string(),
                code: "EPUBCHECK-REPORT".to_string(),
                location: None,
                text: format!(
                    "EPUBCheck did not produce a readable JSON report: {error}. stderr: {}",
                    stderr.trim()
                ),
            }],
        },
    }
}

fn unavailable_epubcheck(message: String) -> EpubCheckReport {
    EpubCheckReport {
        ran: false,
        version: None,
        status: "unavailable".to_string(),
        messages: vec![ValidationMessage {
            severity: "warning".to_string(),
            code: "EPUBCHECK-UNAVAILABLE".to_string(),
            location: None,
            text: message,
        }],
    }
}

fn discover_epubcheck() -> std::result::Result<EpubCheckCommand, String> {
    if let Some(configured) = env::var_os("BOOKFORGE_EPUBCHECK") {
        return command_from_configured_path(PathBuf::from(configured));
    }

    if let Some(path) = find_on_path("epubcheck") {
        return command_for_path(path);
    }
    Err("epubcheck was not found on PATH and BOOKFORGE_EPUBCHECK is not set".to_string())
}

fn command_from_configured_path(path: PathBuf) -> std::result::Result<EpubCheckCommand, String> {
    if path.is_dir() {
        for name in executable_names("epubcheck") {
            let candidate = path.join(name);
            if candidate.is_file() {
                return command_for_path(candidate);
            }
        }
        return Err(format!(
            "BOOKFORGE_EPUBCHECK directory contains no epubcheck executable: {}",
            path.display()
        ));
    }
    if !path.is_file() {
        if path.components().count() == 1
            && let Some(found) = find_on_path(&path.to_string_lossy())
        {
            return command_for_path(found);
        }
        return Err(format!(
            "BOOKFORGE_EPUBCHECK does not point to a file or directory: {}",
            path.display()
        ));
    }
    command_for_path(path)
}

fn command_for_path(path: PathBuf) -> std::result::Result<EpubCheckCommand, String> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "jar" => {
            let java = find_on_path("java").ok_or_else(|| {
                "EPUBCheck JAR configured but java was not found on PATH".to_string()
            })?;
            Ok(EpubCheckCommand::JavaJar { java, jar: path })
        }
        "bat" | "cmd" if cfg!(windows) => Ok(EpubCheckCommand::WindowsScript(path)),
        _ => Ok(EpubCheckCommand::Direct(path)),
    }
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let paths = env::var_os("PATH")?;
    for directory in env::split_paths(&paths) {
        for executable in executable_names(name) {
            let candidate = directory.join(executable);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn executable_names(name: &str) -> Vec<String> {
    if cfg!(windows) {
        vec![
            format!("{name}.exe"),
            format!("{name}.cmd"),
            format!("{name}.bat"),
            name.to_string(),
        ]
    } else {
        vec![name.to_string()]
    }
}

fn parse_version_banner(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let marker = line.find("EPUBCheck v")?;
        line[marker + "EPUBCheck v".len()..]
            .split_whitespace()
            .next()
            .map(|value| value.trim().to_string())
    })
}

fn parse_epubcheck_json(
    text: &str,
    fallback_version: Option<String>,
) -> std::result::Result<EpubCheckReport, serde_json::Error> {
    let parsed: Value = serde_json::from_str(text.trim())?;
    let checker = parsed.get("checker").unwrap_or(&Value::Null);
    let version = checker
        .get("checkerVersion")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or(fallback_version);
    let mut messages = Vec::new();

    for message in parsed
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let severity = message
            .get("severity")
            .and_then(Value::as_str)
            .unwrap_or("INFO")
            .to_ascii_lowercase();
        let code = message
            .get("ID")
            .and_then(Value::as_str)
            .unwrap_or("EPUBCHECK")
            .to_string();
        let text = message
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("EPUBCheck message")
            .to_string();
        let locations = message.get("locations").and_then(Value::as_array);
        if let Some(locations) = locations
            && !locations.is_empty()
        {
            for location in locations {
                messages.push(ValidationMessage {
                    severity: severity.clone(),
                    code: code.clone(),
                    location: format_location(location),
                    text: text.clone(),
                });
            }
        } else {
            messages.push(ValidationMessage {
                severity,
                code,
                location: None,
                text,
            });
        }
    }

    let fatal_count = checker.get("nFatal").and_then(Value::as_u64).unwrap_or(0);
    let error_count = checker.get("nError").and_then(Value::as_u64).unwrap_or(0);
    let warning_count = checker.get("nWarning").and_then(Value::as_u64).unwrap_or(0);
    let status = if fatal_count > 0
        || error_count > 0
        || messages
            .iter()
            .any(|message| matches!(message.severity.as_str(), "fatal" | "error"))
    {
        "errors"
    } else if warning_count > 0 || messages.iter().any(|message| message.severity == "warning") {
        "warnings"
    } else {
        "valid"
    };

    Ok(EpubCheckReport {
        ran: true,
        version,
        status: status.to_string(),
        messages,
    })
}

fn format_location(location: &Value) -> Option<String> {
    let path = location.get("path").and_then(Value::as_str)?;
    let line = location.get("line").and_then(Value::as_i64);
    let column = location.get("column").and_then(Value::as_i64);
    match (line, column) {
        (Some(line), Some(column)) if line >= 0 && column >= 0 => {
            Some(format!("{path}({line},{column})"))
        }
        _ => Some(path.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn validator_report(segment_count: Option<usize>) -> BookforgeValidatorReport {
        BookforgeValidatorReport {
            status: "valid".to_string(),
            xml_valid: true,
            package_path: Some("content.opf".to_string()),
            spine_count: Some(1),
            xhtml_spine_count: Some(1),
            section_count: Some(1),
            block_count: Some(2),
            default_segmentation_count: segment_count,
            default_segmentation_max_tokens: SegmentationConfig::default().max_segment_tokens,
            default_segmentation_context_tokens: SegmentationConfig::default().context_tokens,
            estimated_token_count: Some(10),
            files_checked: 1,
            messages: Vec::new(),
        }
    }

    #[test]
    fn parses_epubcheck_json_messages_and_status() {
        let report = parse_epubcheck_json(
            r#"{
  "checker": {
    "checkerVersion": "5.3.0",
    "nFatal": 0,
    "nError": 0,
    "nWarning": 1
  },
  "messages": [{
    "ID": "RSC-005",
    "severity": "WARNING",
    "message": "Example warning",
    "locations": [{"path": "OEBPS/chapter.xhtml", "line": 12, "column": 4}]
  }]
}"#,
            None,
        )
        .expect("JSON should parse");

        assert_eq!(report.version.as_deref(), Some("5.3.0"));
        assert_eq!(report.status, "warnings");
        assert_eq!(report.messages.len(), 1);
        assert_eq!(
            report.messages[0].location.as_deref(),
            Some("OEBPS/chapter.xhtml(12,4)")
        );
    }

    #[test]
    fn warning_only_epubcheck_fails_only_in_strict_mode() {
        assert!(!validation_failed("valid", "warnings", false));
        assert!(validation_failed("valid", "warnings", true));
        assert!(validation_failed("errors", "valid", false));
    }

    #[test]
    fn default_report_does_not_collide_with_translation_report() {
        let path = default_report_path(Path::new("book.it.epub"));
        assert_eq!(path, PathBuf::from("book.it.validation.json"));
    }

    #[test]
    fn schema_3_names_validation_resegmentation_explicitly() {
        let report = ValidationReport {
            schema_version: VALIDATION_REPORT_SCHEMA_VERSION,
            epub_path: "book.epub".to_string(),
            epubcheck: unavailable_epubcheck("not installed".to_string()),
            bookforge_validators: validator_report(Some(7)),
        };

        let value = serde_json::to_value(report).expect("report should serialize");
        let validators = value["bookforge_validators"]
            .as_object()
            .expect("validators should be an object");

        assert_eq!(value["schema_version"], 3);
        assert_eq!(validators["default_segmentation_count"], 7);
        assert!(!validators.contains_key("segment_count"));
        assert_eq!(
            validators["default_segmentation_max_tokens"],
            SegmentationConfig::default().max_segment_tokens
        );
    }

    #[test]
    fn schema_2_segment_count_remains_readable() {
        let mut value =
            serde_json::to_value(validator_report(Some(7))).expect("report should serialize");
        let validators = value
            .as_object_mut()
            .expect("validators should be an object");
        validators.remove("default_segmentation_max_tokens");
        validators.remove("default_segmentation_context_tokens");
        let count = validators
            .remove("default_segmentation_count")
            .expect("new count should be present");
        validators.insert("segment_count".to_string(), count);

        let parsed: BookforgeValidatorReport =
            serde_json::from_value(value).expect("schema 2 validator report should deserialize");

        assert_eq!(parsed.default_segmentation_count, Some(7));
        assert_eq!(
            parsed.default_segmentation_max_tokens,
            SegmentationConfig::default().max_segment_tokens
        );
        assert_eq!(
            parsed.default_segmentation_context_tokens,
            SegmentationConfig::default().context_tokens
        );
    }

    #[test]
    fn version_banner_parser_accepts_current_output() {
        assert_eq!(
            parse_version_banner("EPUBCheck v5.3.0\nMessages: 0 errors"),
            Some("5.3.0".to_string())
        );
    }

    // -----------------------------------------------------------------------
    // EPUBCheck subprocess hardening. These tests re-exec the current test
    // binary as a stand-in "EPUBCheck" so deadline/kill, output-limit and
    // environment-scrubbing behaviour runs deterministically on any OS
    // without a real Java/EPUBCheck install (mirroring the tools.rs pattern).
    // -----------------------------------------------------------------------

    const SLEEPING_EPUBCHECK_STAND_IN: &str =
        "commands::validate::tests::fake_epubcheck_sleeps_then_marks_completion";
    const SPAMMING_EPUBCHECK_STAND_IN: &str =
        "commands::validate::tests::fake_epubcheck_spams_both_streams";

    fn epubcheck_stand_in_command(test_name: &str) -> Command {
        let executable = std::env::current_exe().expect("current test executable");
        let mut command = Command::new(executable);
        command.args(["--ignored", "--exact", test_name, "--nocapture"]);
        command
    }

    #[test]
    #[ignore = "stand-in child invoked by the bounded-capture timeout test"]
    fn fake_epubcheck_sleeps_then_marks_completion() {
        // Sleeps well past any deadline the parent test uses, then writes a
        // marker. If the parent's kill failed, the marker appears and the
        // parent test fails; if it was killed, the marker never appears.
        thread::sleep(Duration::from_millis(1_500));
        fs::write("epubcheck-fake-completed", b"completed").expect("completion marker writes");
    }

    #[test]
    #[ignore = "stand-in child invoked by the bounded-capture output-limit test"]
    fn fake_epubcheck_spams_both_streams() {
        use std::io::Write as _;

        let bytes = vec![b'x'; 64 * 1024];
        io::stdout()
            .write_all(&bytes)
            .and_then(|()| io::stdout().flush())
            .expect("stand-in stdout writes");
        io::stderr()
            .write_all(&bytes)
            .and_then(|()| io::stderr().flush())
            .expect("stand-in stderr writes");
    }

    #[test]
    fn timed_out_epubcheck_child_is_killed_and_reaped() {
        let dir = tempfile::tempdir().expect("temp dir should be created");
        let mut command = epubcheck_stand_in_command(SLEEPING_EPUBCHECK_STAND_IN);
        command.current_dir(dir.path());

        let started = Instant::now();
        let capture = run_epubcheck_bounded(command, Duration::from_millis(250), 4096)
            .expect("stand-in should run and time out");
        let elapsed = started.elapsed();

        assert!(capture.timed_out, "deadline must be enforced");
        assert!(
            elapsed < Duration::from_secs(5),
            "deadline kill must be prompt, took {elapsed:?}"
        );
        // Give the stand-in enough time to have written its marker had it
        // survived: 250 ms deadline vs a 1.5 s sleep.
        thread::sleep(Duration::from_millis(1_400));
        assert!(
            !dir.path().join("epubcheck-fake-completed").exists(),
            "a timed-out EPUBCheck must be killed and reaped, not left running"
        );
    }

    #[test]
    fn oversized_epubcheck_output_is_retained_within_the_bound() {
        let dir = tempfile::tempdir().expect("temp dir should be created");
        let mut command = epubcheck_stand_in_command(SPAMMING_EPUBCHECK_STAND_IN);
        command.current_dir(dir.path());

        let capture = run_epubcheck_bounded(command, Duration::from_secs(30), 4096)
            .expect("stand-in should exit normally");
        assert!(!capture.timed_out);
        assert!(
            capture.status.is_some_and(|status| status.success()),
            "a bounded child that exits normally must report success"
        );
        assert!(
            capture.stdout.len() <= 4096,
            "stdout retention must stay bounded"
        );
        assert!(
            capture.stderr.len() <= 4096,
            "stderr retention must stay bounded"
        );
        assert!(
            capture.stdout_exceeded,
            "spammed stdout should trip the limit"
        );
        assert!(
            capture.stderr_exceeded,
            "spammed stderr should trip the limit"
        );
    }

    #[test]
    fn epubcheck_child_environment_excludes_secrets_but_keeps_allowlist() {
        // Seed the command with credential-named variables (as an inheriting
        // spawn would receive them), then apply the scrubbing allowlist: the
        // secrets must vanish while PATH and the other allowlisted variables
        // present in the parent environment are copied through.
        let mut command = Command::new("epubcheck");
        command.env("OPENAI_API_KEY", "secret-openai");
        command.env("OCR_API_KEY", "secret-ocr");
        command.env("ELEVENLABS_API_KEY", "secret-elevenlabs");
        command.env("BOOKFORGE_EPUBCHECK_TEST_SECRET", "secret-fixture");
        apply_epubcheck_environment_allowlist(&mut command, &mut |name| env::var_os(name));

        let environment = command.get_envs().collect::<Vec<_>>();
        for (name, _) in &environment {
            let name = name.to_string_lossy();
            assert!(
                !name.eq_ignore_ascii_case("OPENAI_API_KEY"),
                "{name} leaked"
            );
            assert!(!name.eq_ignore_ascii_case("OCR_API_KEY"), "{name} leaked");
            assert!(
                !name.eq_ignore_ascii_case("ELEVENLABS_API_KEY"),
                "{name} leaked"
            );
            assert!(
                !name.eq_ignore_ascii_case("BOOKFORGE_EPUBCHECK_TEST_SECRET"),
                "{name} leaked"
            );
        }

        let names = environment
            .iter()
            .map(|(name, _)| name.to_string_lossy().to_ascii_uppercase())
            .collect::<Vec<_>>();
        assert!(
            !names.iter().any(|name| name == "OPENAI_API_KEY"),
            "secrets must not be re-added by the allowlist"
        );
        assert!(
            names.iter().any(|name| name == "PATH"),
            "PATH must survive scrubbing so EPUBCheck/Java/scripts can still run"
        );
    }

    #[test]
    fn bounded_capture_reports_success_for_a_fast_exit() {
        let dir = tempfile::tempdir().expect("temp dir should be created");
        let mut command = epubcheck_stand_in_command(SPAMMING_EPUBCHECK_STAND_IN);
        command.current_dir(dir.path());

        let capture = run_epubcheck_bounded(command, Duration::from_secs(30), 1_024 * 1024)
            .expect("stand-in should exit normally");
        assert!(!capture.timed_out);
        assert!(
            !capture.stdout_exceeded,
            "a 1 MiB cap must hold 64 KiB of output"
        );
        assert!(!capture.stderr_exceeded);
        assert!(
            capture.stdout.len() >= 64 * 1024 && capture.stdout.len() <= 1024 * 1024,
            "full stdout should be retained up to the cap, got {} bytes",
            capture.stdout.len()
        );
        assert!(
            capture.stderr.len() >= 64 * 1024 && capture.stderr.len() <= 1024 * 1024,
            "full stderr should be retained up to the cap, got {} bytes",
            capture.stderr.len()
        );
    }
}
