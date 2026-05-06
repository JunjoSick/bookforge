use std::path::{Path, PathBuf};

use assert_cmd::Command;
use bookforge_store::JobStore;
use tempfile::TempDir;

fn bookforge() -> Command {
    Command::cargo_bin("bookforge").expect("bookforge binary should be built")
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("cli crate should be under crates/bookforge-cli")
        .to_path_buf()
}

fn fixture_input() -> PathBuf {
    workspace_root().join("test/test.epub")
}

#[test]
fn cli_translate_mock_quiet_writes_output_report_and_events() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let run = translate_quiet(&temp, "mock-prefix-target");

    assert!(run.output.exists(), "translated EPUB should exist");
    assert!(run.events.exists(), "event log should exist");
    assert!(run.report.exists(), "markdown report should exist");
}

#[test]
fn cli_translate_json_mode_emits_valid_jsonl_stdout_and_file_log() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let output = temp.path().join("json.epub");
    let events = temp.path().join("json-events.jsonl");
    let assert = bookforge()
        .current_dir(temp.path())
        .args([
            "translate",
            fixture_input().to_str().unwrap(),
            "--target",
            "Italian",
            "--provider",
            "mock",
            "--model",
            "mock-prefix-target",
            "--profile",
            "v1-fast",
            "--ui",
            "json",
            "--progress-jsonl",
            events.to_str().unwrap(),
            "--out",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    let stdout_events = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()
        .expect("stdout should be valid JSONL");
    assert!(
        stdout_events
            .iter()
            .any(|event| event.get("JobCreated").is_some()),
        "stdout JSONL should include job creation"
    );

    let file_events = read_jsonl(&events);
    assert!(
        file_events
            .iter()
            .any(|event| event.get("TranslationFinished").is_some()),
        "file JSONL should include completion"
    );
}

#[test]
fn cli_status_after_translate_reports_succeeded_job() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let run = translate_quiet(&temp, "mock-prefix-target");

    let assert = bookforge()
        .current_dir(temp.path())
        .args(["status", &run.job_id])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);

    assert!(stdout.contains(&format!("Job: {}", run.job_id)));
    assert!(stdout.contains("Status: succeeded"));
    assert!(stdout.contains("Segments:"));
    assert!(stdout.contains("Output:"));
    assert!(stdout.contains("Event log:"));
    assert!(stdout.contains("Report:"));
    assert!(stdout.contains("Performance:"));
}

#[test]
fn cli_tail_after_translate_prints_recent_events() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let run = translate_quiet(&temp, "mock-prefix-target");

    let assert = bookforge()
        .current_dir(temp.path())
        .args(["tail", &run.job_id, "--last", "3"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);

    assert!(stdout.contains("Last "));
    assert!(stdout.contains("Reconstructed state:"));
}

#[test]
fn cli_tail_json_outputs_valid_jsonl() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let run = translate_quiet(&temp, "mock-prefix-target");

    let assert = bookforge()
        .current_dir(temp.path())
        .args(["tail", &run.job_id, "--last", "5", "--json"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);

    assert!(!stdout.contains("Last "));
    let events = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()
        .expect("tail --json should emit valid JSONL");
    assert!(!events.is_empty(), "tail --json should emit recent events");
}

#[test]
fn cli_resume_missing_job_fails_clearly() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let assert = bookforge()
        .current_dir(temp.path())
        .args(["resume", "job_missing"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);

    assert!(stderr.contains("job 'job_missing' was not found"));
}

#[test]
fn cli_resume_reuses_checkpointed_segments() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let run = translate_quiet(&temp, "mock-prefix-target");
    let resume_events = temp.path().join("resume-events.jsonl");
    let store = JobStore::open(temp.path().join(".bookforge/jobs.sqlite")).expect("store opens");
    let segment_ids = store
        .segment_records(&run.job_id)
        .expect("segments should load")
        .into_iter()
        .map(|record| record.id)
        .collect::<Vec<_>>();
    assert!(!segment_ids.is_empty(), "fixture should produce segments");
    let retry_id = segment_ids[0].clone();
    store
        .mark_segment_failed(&run.job_id, &retry_id, "force resume")
        .expect("segment should be marked failed");

    let resume = bookforge()
        .current_dir(temp.path())
        .args([
            "resume",
            &run.job_id,
            "--ui",
            "quiet",
            "--progress-jsonl",
            resume_events.to_str().unwrap(),
        ])
        .assert()
        .success();
    assert!(
        resume.get_output().stdout.is_empty(),
        "resume --ui quiet should not write human stdout"
    );

    let events = read_jsonl(&resume_events);
    let segment_finished = events
        .iter()
        .filter_map(|event| event.get("SegmentFinished"))
        .collect::<Vec<_>>();
    assert_eq!(
        segment_finished.len(),
        1,
        "resume should translate only the failed segment"
    );
    assert_eq!(
        segment_finished[0]
            .get("segment_id")
            .and_then(|value| value.as_str()),
        Some(retry_id.as_str())
    );
}

struct TranslateRun {
    job_id: String,
    output: PathBuf,
    events: PathBuf,
    report: PathBuf,
}

fn translate_quiet(temp: &TempDir, model: &str) -> TranslateRun {
    let output = temp.path().join("out.epub");
    let events = temp.path().join("events.jsonl");
    let assert = bookforge()
        .current_dir(temp.path())
        .args([
            "translate",
            fixture_input().to_str().unwrap(),
            "--target",
            "Italian",
            "--provider",
            "mock",
            "--model",
            model,
            "--profile",
            "v1-fast",
            "--ui",
            "quiet",
            "--progress-jsonl",
            events.to_str().unwrap(),
            "--out",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();
    assert!(
        assert.get_output().stdout.is_empty(),
        "translate --ui quiet should not write human stdout"
    );
    let job_id = job_id_from_events(&events);

    TranslateRun {
        job_id,
        report: temp.path().join("out.report.md"),
        output,
        events,
    }
}

fn job_id_from_events(path: &Path) -> String {
    read_jsonl(path)
        .into_iter()
        .find_map(|event| {
            event
                .get("JobCreated")
                .and_then(|payload| payload.get("job_id"))
                .and_then(|value| value.as_str())
                .map(ToOwned::to_owned)
        })
        .expect("event log should include job id")
}

fn read_jsonl(path: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .expect("JSONL file should exist")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("line should be valid JSON"))
        .collect()
}
