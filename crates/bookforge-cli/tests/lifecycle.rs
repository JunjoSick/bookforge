use std::{
    fs,
    path::{Path, PathBuf},
};

use assert_cmd::Command;
use bookforge_store::JobStore;
use sha2::{Digest, Sha256};
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

#[test]
fn cli_resume_uses_input_snapshot_after_original_is_moved() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let input = temp.path().join("source.epub");
    let moved = temp.path().join("source-moved.epub");
    fs::copy(fixture_input(), &input).expect("fixture should copy");
    let run = translate_quiet_input(&temp, &input, "mock-prefix-target");

    let snapshot = temp
        .path()
        .join(".bookforge/runs")
        .join(&run.job_id)
        .join("input.epub");
    let snapshot_sha = temp
        .path()
        .join(".bookforge/runs")
        .join(&run.job_id)
        .join("input.sha256");
    assert!(snapshot.exists(), "input snapshot should exist");
    assert_eq!(
        fs::read_to_string(&snapshot_sha)
            .expect("snapshot sha should read")
            .trim(),
        sha256_file(&snapshot)
    );

    let store = JobStore::open(temp.path().join(".bookforge/jobs.sqlite")).expect("store opens");
    let retry_id = store
        .segment_records(&run.job_id)
        .expect("segments should load")
        .into_iter()
        .next()
        .expect("fixture should have a segment")
        .id;
    store
        .mark_segment_failed(&run.job_id, &retry_id, "force resume")
        .expect("segment should mark failed");
    drop(store);
    fs::rename(&input, &moved).expect("input should move");

    bookforge()
        .current_dir(temp.path())
        .args(["resume", &run.job_id, "--ui", "quiet"])
        .assert()
        .success();
}

#[test]
fn cli_review_generates_artifacts_and_ingest_flags_marks_retry() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let run = translate_quiet(&temp, "mock-prefix-target");
    let review_dir = temp.path().join("review-out");

    bookforge()
        .current_dir(temp.path())
        .args(["review", &run.job_id, "--out", review_dir.to_str().unwrap()])
        .assert()
        .success();

    let review_json_path = review_dir.join("review.json");
    let review_html_path = review_dir.join("index.html");
    assert!(review_json_path.exists(), "review.json should exist");
    assert!(review_html_path.exists(), "index.html should exist");
    let review_json: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&review_json_path).expect("review JSON should read"),
    )
    .expect("review JSON should parse");
    let segments = review_json
        .get("segments")
        .and_then(|value| value.as_array())
        .expect("segments array should exist");
    assert!(!segments.is_empty(), "review should contain segments");
    let sum_input = segments
        .iter()
        .map(|segment| {
            segment
                .pointer("/tokens/input")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
        })
        .sum::<u64>();
    let total_input = review_json
        .pointer("/totals/tokens_input")
        .and_then(|value| value.as_u64())
        .expect("total input tokens should exist");
    assert_eq!(total_input, sum_input);
    assert!(
        fs::read_to_string(&review_html_path)
            .expect("review HTML should read")
            .contains("This page contains the full text of your book. Treat as private.")
    );

    let first_segment = segments[0]
        .get("segment_id")
        .and_then(|value| value.as_str())
        .expect("segment id should exist");
    let flags_path = temp.path().join("flags.json");
    fs::write(
        &flags_path,
        serde_json::to_string_pretty(&serde_json::json!({
            "schema_version": 1,
            "job_id": run.job_id.clone(),
            "exported_at": "2026-05-06T13:45:00Z",
            "flags": [{
                "segment_id": first_segment,
                "kind": "wrong_translation",
                "note": "Meaning is reversed.",
                "suggested_source": null,
                "suggested_target": null
            }]
        }))
        .unwrap(),
    )
    .expect("flags should write");

    bookforge()
        .current_dir(temp.path())
        .args([
            "ingest-flags",
            &run.job_id,
            "--flags",
            flags_path.to_str().unwrap(),
        ])
        .assert()
        .success();

    let store = JobStore::open(temp.path().join(".bookforge/jobs.sqlite")).expect("store opens");
    assert_eq!(
        store
            .segment_flag_count(&run.job_id)
            .expect("flag count should load"),
        1
    );
    let summary = store
        .summary(&run.job_id)
        .expect("summary should load")
        .expect("job should exist");
    assert_eq!(summary.needs_review, 1);
    drop(store);

    bookforge()
        .current_dir(temp.path())
        .args(["retry", &run.job_id, "--only", "needs-review"])
        .assert()
        .success();

    let store = JobStore::open(temp.path().join(".bookforge/jobs.sqlite")).expect("store opens");
    let summary = store
        .summary(&run.job_id)
        .expect("summary should load")
        .expect("job should exist");
    assert_eq!(summary.retry_pending, 1);
}

struct TranslateRun {
    job_id: String,
    output: PathBuf,
    events: PathBuf,
    report: PathBuf,
}

fn translate_quiet(temp: &TempDir, model: &str) -> TranslateRun {
    translate_quiet_input(temp, &fixture_input(), model)
}

fn translate_quiet_input(temp: &TempDir, input: &Path, model: &str) -> TranslateRun {
    let output = temp.path().join("out.epub");
    let events = temp.path().join("events.jsonl");
    let assert = bookforge()
        .current_dir(temp.path())
        .args([
            "translate",
            input.to_str().unwrap(),
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

fn sha256_file(path: &Path) -> String {
    let bytes = fs::read(path).expect("file should read");
    let digest = Sha256::digest(&bytes);
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing hash should not fail");
    }
    output
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
