//! Double-check honors its provider, model, endpoint and key overrides on a
//! fresh translation, against two local fixture endpoints. No network access.

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use assert_cmd::Command;
use serde_json::{Value, json};

#[derive(Clone, Debug)]
struct Request {
    review: bool,
    model: String,
    authorization: String,
}

struct ProviderFixture {
    address: SocketAddr,
    calls: Arc<Mutex<Vec<Request>>>,
    stopped: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl ProviderFixture {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let stopped = Arc::new(AtomicBool::new(false));
        let recorded = calls.clone();
        let stop = stopped.clone();
        let worker = thread::spawn(move || {
            // Keep answered sockets until the fixture is dropped; an early
            // close can reset a Windows loopback response before it is read.
            let mut retained = Vec::new();
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                if stop.load(Ordering::Acquire) {
                    break;
                }
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut reader = BufReader::new(&stream);
                let mut content_length = 0;
                let mut authorization = String::new();
                loop {
                    let mut line = String::new();
                    assert!(
                        reader.read_line(&mut line).unwrap() > 0,
                        "truncated headers"
                    );
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        if name.eq_ignore_ascii_case("content-length") {
                            content_length = value.trim().parse().unwrap();
                        } else if name.eq_ignore_ascii_case("authorization") {
                            authorization = value.trim().to_owned();
                        }
                    }
                }
                let mut body = vec![0; content_length];
                reader.read_exact(&mut body).unwrap();
                let request: Value = serde_json::from_slice(&body).unwrap();
                let prompt = request["messages"].as_array().unwrap().last().unwrap()["content"]
                    .as_str()
                    .unwrap();
                let items = prompt_items(prompt);
                // Review prompts carry `source`; translation prompts `text`/`runs`.
                let review = items[0].get("source").is_some();
                recorded.lock().unwrap().push(Request {
                    review,
                    model: request["model"].as_str().unwrap().to_owned(),
                    authorization,
                });
                let content = if review {
                    let verdicts = items
                        .iter()
                        .map(|item| json!({"id": item["id"], "verdict": "pass", "issues": []}))
                        .collect::<Vec<_>>();
                    json!({"reviews": verdicts, "items": verdicts})
                } else {
                    json!({"items": items.iter().map(|item| match item.get("runs") {
                        Some(runs) => json!({"id": item["id"], "runs": runs}),
                        None => json!({"id": item["id"], "translation": item["text"]}),
                    }).collect::<Vec<_>>()})
                };
                let response = json!({
                    "choices": [{"message": {"role": "assistant", "content": content.to_string()}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 100, "completion_tokens": 100, "total_tokens": 200},
                })
                .to_string();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response.len(),
                    response
                )
                .unwrap();
                retained.push(stream);
            }
        });
        Self {
            address,
            calls,
            stopped,
            worker: Some(worker),
        }
    }

    fn url(&self) -> String {
        format!("http://{}/v1", self.address)
    }

    fn take_calls(&self) -> Vec<Request> {
        std::mem::take(&mut *self.calls.lock().unwrap())
    }
}

impl Drop for ProviderFixture {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.address);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// The first JSON array of prompt items (objects with an `id` and text).
fn prompt_items(prompt: &str) -> Vec<Value> {
    prompt
        .char_indices()
        .filter(|(_, ch)| *ch == '[')
        .find_map(|(index, _)| {
            let parsed = serde_json::Deserializer::from_str(&prompt[index..])
                .into_iter::<Value>()
                .next()?
                .ok()?;
            let items = parsed.as_array()?;
            (!items.is_empty()
                && items.iter().all(|item| {
                    item.get("id").is_some()
                        && ["source", "text", "runs"]
                            .iter()
                            .any(|key| item.get(*key).is_some())
                }))
            .then(|| items.clone())
        })
        .expect("request has prompt items")
}

fn build_epub(path: &Path) {
    let mut archive = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    for (name, data) in [
        ("mimetype", "application/epub+zip"),
        (
            "META-INF/container.xml",
            r#"<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#,
        ),
        (
            "content.opf",
            r#"<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="id">review-fixture</dc:identifier><dc:title>Review fixture</dc:title><dc:language>en</dc:language></metadata><manifest><item id="chapter" href="chapter.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="chapter"/></spine></package>"#,
        ),
        (
            "chapter.xhtml",
            r#"<html xmlns="http://www.w3.org/1999/xhtml"><head></head><body><p>This local paragraph checks that translation and review use their independently configured providers.</p></body></html>"#,
        ),
    ] {
        archive
            .start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        archive.write_all(data.as_bytes()).unwrap();
    }
    archive.finish().unwrap();
}

fn translate_command(workdir: &Path, input: &Path, primary: &ProviderFixture) -> Command {
    let mut command = Command::cargo_bin("bookforge").unwrap();
    command
        .current_dir(workdir)
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env("BOOKFORGE_ROUTE_PRIMARY", "local-primary-fixture")
        .env("BOOKFORGE_ROUTE_REVIEW", "local-review-fixture")
        .timeout(Duration::from_secs(60))
        .arg("translate")
        .arg(input)
        .args([
            "--source",
            "en",
            "--target",
            "en",
            "--provider",
            "openai-compatible",
            "--model",
            "translation-fixture",
            "--base-url",
            &primary.url(),
            "--api-key-env",
            "BOOKFORGE_ROUTE_PRIMARY",
            "--profile",
            "balanced",
            "--compact-prompts",
            "true",
            "--context-window",
            "0",
            "--concurrency",
            "1",
            "--adaptive-concurrency",
            "false",
            "--provider-max-attempts",
            "1",
            "--max-attempts",
            "1",
            "--ui",
            "quiet",
        ])
        .arg("--out")
        .arg(workdir.join("output.epub"));
    command
}

#[test]
fn double_check_provider_overrides_apply_on_fresh_translation() {
    for overrides in ["none", "all", "model", "endpoint", "key", "provider", "off"] {
        let workdir = tempfile::tempdir().unwrap();
        let input = workdir.path().join("input.epub");
        build_epub(&input);
        let primary = ProviderFixture::new();
        let reviewer = ProviderFixture::new();
        let mut translate = translate_command(workdir.path(), &input, &primary);
        translate.args([
            "--qa",
            "off",
            "--double-check",
            if overrides == "off" { "off" } else { "full" },
        ]);
        if matches!(overrides, "all" | "provider") {
            // With no endpoint override the saved translation endpoint is
            // inherited: the provider name alone must not reroute requests.
            translate.args(["--double-check-provider", "openai-compatible"]);
        }
        if matches!(overrides, "all" | "model") {
            translate.args(["--double-check-model", "review-fixture"]);
        }
        if matches!(overrides, "all" | "endpoint") {
            translate.args(["--double-check-base-url", &reviewer.url()]);
        }
        if matches!(overrides, "all" | "key") {
            translate.args(["--double-check-api-key-env", "BOOKFORGE_ROUTE_REVIEW"]);
        }
        if overrides == "off" {
            translate.args([
                "--double-check-provider",
                "unused-provider",
                "--double-check-api-key-env",
                "BOOKFORGE_UNUSED_DC_KEY",
            ]);
        }
        translate.assert().success();

        let primary_calls = primary.take_calls();
        let reviewer_calls = reviewer.take_calls();
        assert!(
            primary_calls
                .iter()
                .filter(|call| !call.review)
                .all(|call| call.model == "translation-fixture"
                    && call.authorization == "Bearer local-primary-fixture"),
            "{overrides}: translation left its endpoint: {primary_calls:?}"
        );
        assert!(reviewer_calls.iter().all(|call| call.review));
        if overrides == "off" {
            assert!(primary_calls.iter().all(|call| !call.review));
            assert!(reviewer_calls.is_empty());
            continue;
        }
        let calls: Vec<_> = if matches!(overrides, "all" | "endpoint") {
            assert!(
                primary_calls.iter().all(|call| !call.review),
                "{overrides}: double-check hit the primary endpoint"
            );
            reviewer_calls.iter().collect()
        } else {
            assert!(reviewer_calls.is_empty(), "{overrides}: {reviewer_calls:?}");
            primary_calls.iter().filter(|call| call.review).collect()
        };
        assert!(!calls.is_empty(), "{overrides}: missing double-check calls");
        let model = if matches!(overrides, "all" | "model") {
            "review-fixture"
        } else {
            "translation-fixture"
        };
        let key = if matches!(overrides, "all" | "key") {
            "Bearer local-review-fixture"
        } else {
            "Bearer local-primary-fixture"
        };
        assert!(
            calls
                .iter()
                .all(|call| call.model == model && call.authorization == key),
            "{overrides}: {calls:?}"
        );
    }
}
