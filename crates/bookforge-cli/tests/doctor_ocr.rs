use std::{
    io::{Read, Write},
    net::{Shutdown, TcpListener},
    sync::mpsc,
    time::Duration,
};

use assert_cmd::Command;
use predicates::str::contains;

const TEST_DEADLOCK_TIMEOUT: Duration = Duration::from_secs(30);

fn bookforge() -> Command {
    Command::cargo_bin("bookforge").expect("bookforge binary should be built")
}

#[test]
fn doctor_lists_models_from_loopback_ocr_endpoint() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("mock OCR listener");
    let address = listener.local_addr().expect("mock OCR address");
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let _ = stream.set_read_timeout(Some(TEST_DEADLOCK_TIMEOUT));
        let mut request = Vec::new();
        let mut scratch = [0u8; 2048];
        loop {
            match stream.read(&mut scratch) {
                Ok(0) | Err(_) => break,
                Ok(read) => request.extend_from_slice(&scratch[..read]),
            }
            if request.windows(4).any(|part| part == b"\r\n\r\n") {
                break;
            }
        }
        // Queue the capture before responding so `recv` can never race the
        // server thread.
        let _ = sender.send(String::from_utf8_lossy(&request).into_owned());
        let body = br#"{"data":[{"id":"baidu/Unlimited-OCR"}]}"#;
        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        if stream.write_all(headers.as_bytes()).is_err() || stream.write_all(body).is_err() {
            return;
        }
        // Close write-side first and drain until the peer finishes so the
        // socket never closes with unread inbound data (Windows answers that
        // with RST instead of FIN, which surfaces to clients).
        let _ = stream.shutdown(Shutdown::Write);
        let mut drain = [0u8; 2048];
        while matches!(stream.read(&mut drain), Ok(read) if read > 0) {}
    });

    bookforge()
        .args(["doctor", "--ocr-endpoint", &format!("http://{address}/v1")])
        .assert()
        .success()
        .stdout(contains("Reachable: yes"))
        .stdout(contains("baidu/Unlimited-OCR"));

    let request = receiver
        .recv_timeout(TEST_DEADLOCK_TIMEOUT)
        .expect("doctor request captured");
    assert!(request.starts_with("GET /v1/models HTTP/1.1"));
}

#[test]
fn doctor_rejects_remote_plain_http_ocr_endpoint_without_connecting() {
    // Remote plain HTTP must be refused before any connection attempt or key
    // lookup, so this stays offline-deterministic even if the endpoint were
    // reachable.
    bookforge()
        .args(["doctor", "--ocr-endpoint", "http://example.com/v1"])
        .assert()
        .failure()
        .stdout(contains("Reachable: no"))
        .stdout(contains("unsafe OCR base URL"));
}
