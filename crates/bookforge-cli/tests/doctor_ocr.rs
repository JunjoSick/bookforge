use std::{
    io::{Read, Write},
    net::TcpListener,
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
        let (mut stream, _) = listener.accept().expect("mock OCR accept");
        stream
            .set_read_timeout(Some(TEST_DEADLOCK_TIMEOUT))
            .expect("read timeout");
        let mut request = Vec::new();
        let mut scratch = [0u8; 2048];
        loop {
            let read = stream.read(&mut scratch).expect("read request");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&scratch[..read]);
            if request.windows(4).any(|part| part == b"\r\n\r\n") {
                break;
            }
        }
        let body = br#"{"data":[{"id":"baidu/Unlimited-OCR"}]}"#;
        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(headers.as_bytes()).expect("write headers");
        stream.write_all(body).expect("write body");
        sender
            .send(String::from_utf8_lossy(&request).into_owned())
            .expect("send request");
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
