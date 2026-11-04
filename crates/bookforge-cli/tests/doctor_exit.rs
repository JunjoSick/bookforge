//! End-to-end exit status of `doctor`: a failed required check exits non-zero
//! (CLI-17), advisory warnings do not, and `--no-fail` keeps the old green
//! exit. Every endpoint is a loopback fixture; no real provider is contacted.

use std::{
    io::{Read, Write},
    net::{Shutdown, TcpListener},
    time::Duration,
};

use assert_cmd::Command;
use predicates::str::contains;

fn bookforge() -> Command {
    let mut command = Command::cargo_bin("bookforge").expect("bookforge binary should be built");
    command.timeout(Duration::from_secs(60));
    command
}

/// Serve exactly one HTTP response with `status` and a JSON `body`.
fn endpoint(status: &str, body: &str) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let status = status.to_owned();
    let body = body.to_owned();
    let server = std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(20)));
        let mut request = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            let read = match stream.read(&mut buffer) {
                Ok(0) | Err(_) => return,
                Ok(read) => read,
            };
            request.extend_from_slice(&buffer[..read]);
            if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                let length = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if request.len() >= end + 4 + length {
                    break;
                }
            }
        }
        let _ = write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        // Close the write side and drain, so Windows answers with FIN rather
        // than RST while the client still reads.
        let _ = stream.shutdown(Shutdown::Write);
        while matches!(stream.read(&mut buffer), Ok(read) if read > 0) {}
    });
    (url, server)
}

fn provider_doctor(url: &str) -> Command {
    let mut command = bookforge();
    command
        .env("BOOKFORGE_DOCTOR_TEST_KEY", "fixture-key")
        .args([
            "doctor",
            "--provider",
            "openai-compatible",
            "--base-url",
            url,
            "--api-key-env",
            "BOOKFORGE_DOCTOR_TEST_KEY",
            "--timeout-seconds",
            "10",
        ]);
    command
}

#[test]
fn doctor_failed_provider_completion_exits_nonzero() {
    let (url, server) = endpoint(
        "401 Unauthorized",
        r#"{"error":{"message":"fixture failure"}}"#,
    );
    provider_doctor(&url)
        .assert()
        .code(1)
        .stdout(contains("Completion: FAILED"))
        .stderr(contains("doctor check(s) failed: provider"));
    server.join().unwrap();
}

#[test]
fn doctor_no_fail_keeps_a_failed_check_green() {
    let (url, server) = endpoint(
        "401 Unauthorized",
        r#"{"error":{"message":"fixture failure"}}"#,
    );
    provider_doctor(&url)
        .arg("--no-fail")
        .assert()
        .success()
        .stdout(contains("Completion: FAILED"));
    server.join().unwrap();
}

#[test]
fn doctor_successful_completion_keeps_advisory_warnings_nonfatal() {
    let (url, server) = endpoint(
        "200 OK",
        r#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}]}"#,
    );
    provider_doctor(&url)
        .assert()
        .success()
        .stdout(contains("JSON response_format: may not be supported"))
        .stdout(contains("Usage tokens: not reported"));
    server.join().unwrap();
}

#[test]
fn doctor_missing_provider_key_exits_nonzero() {
    bookforge()
        .env_remove("BOOKFORGE_DOCTOR_TEST_MISSING_KEY")
        .args([
            "doctor",
            "--provider",
            "openai-compatible",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--api-key-env",
            "BOOKFORGE_DOCTOR_TEST_MISSING_KEY",
        ])
        .assert()
        .code(1)
        .stdout(contains("MISSING"))
        .stderr(contains("doctor check(s) failed: provider"));
}

#[test]
fn doctor_failed_ocr_check_exits_nonzero() {
    let (url, server) = endpoint("503 Service Unavailable", r#"{"error":"fixture failure"}"#);
    bookforge()
        .args(["doctor", "--ocr-endpoint", &url, "--timeout-seconds", "10"])
        .assert()
        .code(1)
        .stdout(contains("Reachable: no"))
        .stderr(contains("doctor check(s) failed: ocr"));
    server.join().unwrap();
}

#[test]
fn doctor_missing_required_pdf_tools_exits_nonzero() {
    let directory = tempfile::tempdir().unwrap();
    bookforge()
        .current_dir(directory.path())
        .env("PATH", directory.path())
        .env_remove("POPPLER_PATH")
        .env_remove("BOOKFORGE_POPPLER_PATH")
        .args(["doctor", "--pdf"])
        .assert()
        .code(1)
        .stdout(contains("MISSING"))
        .stderr(contains("doctor check(s) failed: pdf"));
}

#[test]
fn doctor_absent_storage_succeeds_but_corrupt_storage_fails() {
    let directory = tempfile::tempdir().unwrap();
    bookforge()
        .current_dir(directory.path())
        .args(["doctor", "--storage"])
        .assert()
        .success()
        .stdout(contains("No storage issues to report"));
    let storage = directory.path().join(".bookforge");
    std::fs::create_dir(&storage).unwrap();
    std::fs::write(storage.join("jobs.sqlite"), b"not a SQLite database").unwrap();
    bookforge()
        .current_dir(directory.path())
        .args(["doctor", "--storage"])
        .assert()
        .code(1);
}
