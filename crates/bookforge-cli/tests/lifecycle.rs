use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

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

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}
