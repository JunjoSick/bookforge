use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use anyhow::{Context, Result, bail};
use bookforge_core::{config::SegmentationConfig, segment::build_segments};
use bookforge_epub::{ValidationSeverity, inspect_epub, read_epub, validate_translated_epub};
use clap::Args;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const VALIDATION_REPORT_SCHEMA_VERSION: u32 = 3;

#[derive(Debug, Args)]
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
    let outcome = validate_path(input, strict_epubcheck);
    if let Some(parent) = report_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating report directory {}", parent.display()))?;
    }
    fs::write(report_path, serde_json::to_string_pretty(&outcome.report)?)
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
    fn output(&self, args: &[&str], input: Option<&Path>) -> std::io::Result<Output> {
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
        command.args(args);
        if let Some(input) = input {
            command.arg(input);
        }
        command.output()
    }
}

fn run_epubcheck(input: &Path) -> EpubCheckReport {
    let command = match discover_epubcheck() {
        Ok(command) => command,
        Err(message) => return unavailable_epubcheck(message),
    };

    let version = command
        .output(&["--version"], None)
        .ok()
        .and_then(|output| {
            let text = format!(
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
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
    let output = match command.output(&["--json", &report_arg], Some(input)) {
        Ok(output) => output,
        Err(error) => {
            return unavailable_epubcheck(format!("failed to run EPUBCheck: {error}"));
        }
    };
    let stderr = String::from_utf8_lossy(&output.stderr);
    let report_json = fs::read_to_string(&report_path);
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
            if report.messages.is_empty() && !output.status.success() {
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
}
