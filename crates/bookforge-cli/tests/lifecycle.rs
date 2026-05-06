use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use bookforge_store::JobStore;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_bookforge"))
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("cli crate should be under crates/bookforge-cli")
        .to_path_buf()
}

fn temp_dir(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "bookforge-cli-{name}-{}-{}",
        std::process::id(),
        now_nanos()
    ));
    fs::create_dir_all(&path).expect("temp dir should be created");
    path
}

#[test]
fn mock_translate_status_and_tail_lifecycle() {
    let cwd = temp_dir("lifecycle");
    let input = workspace_root().join("test/test.epub");
    let output = cwd.join("translated.epub");
    let events = cwd.join("events.jsonl");

    let translate = Command::new(bin())
        .current_dir(&cwd)
        .args([
            "translate",
            input.to_str().unwrap(),
            "--target",
            "Italian",
            "--provider",
            "mock",
            "--model",
            "mock-prefix-target",
            "--profile",
            "v1-fast",
            "--ui",
            "quiet",
            "--progress-jsonl",
            events.to_str().unwrap(),
            "--out",
            output.to_str().unwrap(),
        ])
        .output()
        .expect("translate command should run");
    assert!(
        translate.status.success(),
        "translate failed: {}",
        String::from_utf8_lossy(&translate.stderr)
    );
    assert!(output.exists(), "translated EPUB should exist");
    assert!(events.exists(), "event log should exist");
    assert!(
        cwd.join("translated.report.md").exists(),
        "report should exist"
    );

    let stdout = String::from_utf8_lossy(&translate.stdout);
    let job_id = stdout
        .lines()
        .find_map(|line| line.strip_prefix("Job: "))
        .expect("translate should print job id")
        .to_string();

    let status = Command::new(bin())
        .current_dir(&cwd)
        .args(["status", &job_id])
        .output()
        .expect("status command should run");
    assert!(status.status.success());
    let status_stdout = String::from_utf8_lossy(&status.stdout);
    assert!(status_stdout.contains("Status: succeeded"));
    assert!(status_stdout.contains("Event log:"));
    assert!(status_stdout.contains("Report:"));
    assert!(status_stdout.contains("Performance:"));

    let tail = Command::new(bin())
        .current_dir(&cwd)
        .args(["tail", &job_id, "--last", "3"])
        .output()
        .expect("tail command should run");
    assert!(tail.status.success());
    assert!(String::from_utf8_lossy(&tail.stdout).contains("Last "));

    let tail_json = Command::new(bin())
        .current_dir(&cwd)
        .args(["tail", &job_id, "--last", "3", "--json"])
        .output()
        .expect("tail json command should run");
    assert!(tail_json.status.success());
    for line in String::from_utf8_lossy(&tail_json.stdout).lines() {
        serde_json::from_str::<serde_json::Value>(line).expect("tail --json should emit JSONL");
    }
}

#[test]
fn resume_missing_job_fails_clearly() {
    let cwd = temp_dir("resume-missing");
    let output = Command::new(bin())
        .current_dir(&cwd)
        .args(["resume", "job_missing"])
        .output()
        .expect("resume command should run");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("job 'job_missing' was not found"));
}

#[test]
fn cli_resume_writes_progress_events_and_uses_batch_snapshot_mode() {
    let cwd = temp_dir("resume-batch");
    let input = workspace_root().join("test/test.epub");
    let output = cwd.join("translated.epub");
    let translate_events = cwd.join("translate-events.jsonl");
    let resume_events = cwd.join("resume-events.jsonl");

    let translate = Command::new(bin())
        .current_dir(&cwd)
        .args([
            "translate",
            input.to_str().unwrap(),
            "--target",
            "Italian",
            "--provider",
            "mock",
            "--model",
            "mock-prefix-target",
            "--profile",
            "v1-fast",
            "--ui",
            "quiet",
            "--progress-jsonl",
            translate_events.to_str().unwrap(),
            "--out",
            output.to_str().unwrap(),
        ])
        .output()
        .expect("translate command should run");
    assert!(
        translate.status.success(),
        "translate failed: {}",
        String::from_utf8_lossy(&translate.stderr)
    );
    let job_id = job_id_from_stdout(&translate.stdout);

    let store = JobStore::open(cwd.join(".bookforge/jobs.sqlite")).expect("store should open");
    let segment_ids = store
        .segment_records(&job_id)
        .expect("segments should load")
        .into_iter()
        .map(|record| record.id)
        .collect::<Vec<_>>();
    assert!(!segment_ids.is_empty(), "fixture should have segments");
    for segment_id in segment_ids {
        store
            .mark_segment_failed(&job_id, &segment_id, "force resume")
            .expect("segment should be marked failed");
    }

    let resume = Command::new(bin())
        .current_dir(&cwd)
        .args([
            "resume",
            &job_id,
            "--ui",
            "quiet",
            "--progress-jsonl",
            resume_events.to_str().unwrap(),
        ])
        .output()
        .expect("resume command should run");
    assert!(
        resume.status.success(),
        "resume failed: {}\nstdout: {}",
        String::from_utf8_lossy(&resume.stderr),
        String::from_utf8_lossy(&resume.stdout)
    );

    let events = read_jsonl(&resume_events);
    assert!(events.iter().any(|event| {
        event
            .get("StageStarted")
            .and_then(|payload| payload.get("stage"))
            .and_then(|stage| stage.as_str())
            == Some("resume")
    }));
    assert!(
        events
            .iter()
            .any(|event| event.get("CacheScanFinished").is_some())
    );
    assert!(
        events
            .iter()
            .any(|event| event.get("BatchQueued").is_some())
            || events.iter().any(|event| event
                .get("RequestStarted")
                .and_then(|payload| payload.get("batch_id"))
                .is_some_and(|batch_id| !batch_id.is_null())),
        "resume should use batch progress events when the snapshot has batch enabled"
    );
    assert!(
        events
            .iter()
            .any(|event| event.get("ArtifactWritten").is_some())
    );
    assert!(
        events
            .iter()
            .any(|event| event.get("TranslationFinished").is_some())
    );

    let summary = store
        .summary(&job_id)
        .expect("summary should load")
        .expect("job should exist");
    assert_eq!(summary.failed, 0);
    assert_eq!(
        summary.succeeded + summary.cached + summary.needs_review,
        summary.total_segments
    );

    let tail = Command::new(bin())
        .current_dir(&cwd)
        .args(["tail", &job_id, "--last", "200", "--json"])
        .output()
        .expect("tail command should run");
    assert!(tail.status.success());
    let resume_lines = fs::read_to_string(&resume_events).expect("resume events should exist");
    let resume_lines = resume_lines.lines().collect::<Vec<_>>();
    let tail_stdout = String::from_utf8_lossy(&tail.stdout);
    let tail_lines = tail_stdout.lines().collect::<Vec<_>>();
    assert!(!tail_lines.is_empty(), "tail should emit JSONL events");
    let expected_start = resume_lines.len().saturating_sub(tail_lines.len());
    assert_eq!(
        tail_lines[0], resume_lines[expected_start],
        "tail should read the resume event path stored by --progress-jsonl"
    );
}

#[test]
fn resume_preserves_needs_review_segments_by_default() {
    let cwd = temp_dir("resume-needs-review");
    let input = workspace_root().join("test/test.epub");
    let output = cwd.join("translated.epub");
    let resume_events = cwd.join("resume-events.jsonl");

    let translate = Command::new(bin())
        .current_dir(&cwd)
        .args([
            "translate",
            input.to_str().unwrap(),
            "--target",
            "Italian",
            "--provider",
            "mock",
            "--model",
            "mock-malformed-json",
            "--profile",
            "v1-fast",
            "--ui",
            "quiet",
            "--out",
            output.to_str().unwrap(),
        ])
        .output()
        .expect("translate command should run");
    assert!(
        translate.status.success(),
        "translate failed: {}",
        String::from_utf8_lossy(&translate.stderr)
    );
    let job_id = job_id_from_stdout(&translate.stdout);

    let resume = Command::new(bin())
        .current_dir(&cwd)
        .args([
            "resume",
            &job_id,
            "--ui",
            "quiet",
            "--progress-jsonl",
            resume_events.to_str().unwrap(),
        ])
        .output()
        .expect("resume command should run");
    assert!(
        resume.status.success(),
        "resume failed: {}",
        String::from_utf8_lossy(&resume.stderr)
    );

    let store = JobStore::open(cwd.join(".bookforge/jobs.sqlite")).expect("store should open");
    let summary = store
        .summary(&job_id)
        .expect("summary should load")
        .expect("job should exist");
    assert_eq!(summary.needs_review, summary.total_segments);
    assert_eq!(summary.failed, 0);

    let events = read_jsonl(&resume_events);
    assert!(
        events
            .iter()
            .all(|event| event.get("SegmentFinished").is_none()),
        "resume should not reprocess needs_review segments by default"
    );
}

fn job_id_from_stdout(stdout: &[u8]) -> String {
    String::from_utf8_lossy(stdout)
        .lines()
        .find_map(|line| line.strip_prefix("Job: "))
        .expect("command should print job id")
        .to_string()
}

fn read_jsonl(path: &Path) -> Vec<serde_json::Value> {
    fs::read_to_string(path)
        .expect("JSONL file should exist")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("line should be valid JSON"))
        .collect()
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}
