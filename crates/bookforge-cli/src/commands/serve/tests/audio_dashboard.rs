//! Dashboard audiobook coverage: estimates, cancellation, and artifact
//! publication. Fixtures are synthetic and every provider stays offline.

use super::*;
use audiobook::AudioProviderKind;
use std::io::Write as _;

const HEADING_TAG_SUFFIX: &str = " <break time=\"0.6s\" />";

fn chapter_xhtml(heading: &str, paragraph: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<head></head>
<body>
<h1>{heading}</h1>
<p>{paragraph}</p>
</body>
</html>"#
    )
}

/// Two chapters, each opening with an `h1` that becomes its title chunk.
fn build_headed_fixture_epub(path: &Path) {
    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    let mut zip = ZipWriter::new(std::fs::File::create(path).unwrap());
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    zip.start_file("mimetype", stored).unwrap();
    zip.write_all(b"application/epub+zip").unwrap();
    zip.start_file("META-INF/container.xml", stored).unwrap();
    zip.write_all(FIXTURE_CONTAINER_XML.as_bytes()).unwrap();
    zip.start_file("content.opf", stored).unwrap();
    zip.write_all(FIXTURE_OPF.as_bytes()).unwrap();
    zip.start_file("chapter1.xhtml", stored).unwrap();
    zip.write_all(
        chapter_xhtml(
            "Opening Notes",
            "A gray heron waited beside the canal while the ferry crossed.",
        )
        .as_bytes(),
    )
    .unwrap();
    zip.start_file("chapter2.xhtml", stored).unwrap();
    zip.write_all(
        chapter_xhtml(
            "Closing Notes",
            "By evening the market stalls were folded and the square was quiet.",
        )
        .as_bytes(),
    )
    .unwrap();
    zip.finish().unwrap();
}

#[test]
fn audiobook_estimate_counts_model_heading_breaks_and_chapter_filter() {
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("book.epub");
    build_headed_fixture_epub(&input);
    let estimate = |model: &str, filter: Option<std::collections::BTreeSet<usize>>| {
        plan_audiobook_estimate(
            &input,
            temp.path(),
            AudioProviderKind::Elevenlabs,
            model,
            2_000,
            filter,
        )
        .unwrap()
    };

    // eleven_v3 never receives break tags; Flash v2.5 does under `auto`.
    let plain = estimate("eleven_v3", None);
    let tagged = estimate("eleven_flash_v2_5", None);
    assert_eq!(plain.0, 2, "{plain:?}");
    assert_eq!((tagged.0, tagged.1), (plain.0, plain.1));
    assert_eq!(
        tagged.2 - plain.2,
        2 * HEADING_TAG_SUFFIX.chars().count(),
        "each chapter title carries exactly one break tag"
    );

    // An Auto estimate has no resolved model yet, so it carries no tags.
    assert_eq!(estimate("", None), plain);

    let filtered = estimate("eleven_flash_v2_5", Some([2].into()));
    assert_eq!(filtered.0, 1);
    assert!(filtered.1 < tagged.1);
    assert!(filtered.2 < tagged.2);
}

#[test]
fn audiobook_estimate_rejects_heading_break_without_text_room() {
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("book.epub");
    build_headed_fixture_epub(&input);
    // The break tag plus its separating space is 22 scalars: a 22-character
    // budget leaves no room for any heading text.
    let error = plan_audiobook_estimate(
        &input,
        temp.path(),
        AudioProviderKind::Elevenlabs,
        "eleven_flash_v2_5",
        22,
        None,
    )
    .unwrap_err();
    assert!(error.to_string().contains("heading break tag"), "{error:#}");
    // A model without break tags can use the same small budget.
    plan_audiobook_estimate(
        &input,
        temp.path(),
        AudioProviderKind::Elevenlabs,
        "eleven_v3",
        22,
        None,
    )
    .unwrap();
}

#[test]
fn audiobook_estimate_rejects_a_book_without_narratable_text() {
    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("empty.epub");
    let mut zip = ZipWriter::new(std::fs::File::create(&input).unwrap());
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    zip.start_file("mimetype", stored).unwrap();
    zip.write_all(b"application/epub+zip").unwrap();
    zip.start_file("META-INF/container.xml", stored).unwrap();
    zip.write_all(FIXTURE_CONTAINER_XML.as_bytes()).unwrap();
    zip.start_file("content.opf", stored).unwrap();
    zip.write_all(FIXTURE_OPF.as_bytes()).unwrap();
    for name in ["chapter1.xhtml", "chapter2.xhtml"] {
        zip.start_file(name, stored).unwrap();
        zip.write_all(
            br#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head></head><body></body></html>"#,
        )
        .unwrap();
    }
    zip.finish().unwrap();

    let error = plan_audiobook_estimate(
        &input,
        temp.path(),
        AudioProviderKind::Mock,
        "mock-silence",
        2_000,
        None,
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("no narratable text"),
        "{error:#}"
    );
}

#[test]
fn audiobook_quota_fits_uses_estimated_credits_with_rounding() {
    let flash =
        crate::audio_cost::estimate_audio_cost("elevenlabs", "eleven_flash_v2_5", 5_000).unwrap();
    let credits = flash
        .credits
        .expect("the bundled table prices Flash in credits");
    assert!(
        credits < 5_000.0,
        "Flash bills below one credit per character"
    );
    let needed = credits.ceil() as u64;
    // Credits, not characters, decide the fit when a price exists.
    assert!(audiobook::elevenlabs_quota_fits(
        5_000,
        Some(credits),
        needed
    ));
    assert!(!audiobook::elevenlabs_quota_fits(
        5_000,
        Some(credits),
        needed - 1
    ));
    // Fractional credits round up before comparison.
    assert!(audiobook::elevenlabs_quota_fits(5_000, Some(700.0), 700));
    assert!(!audiobook::elevenlabs_quota_fits(5_000, Some(700.1), 700));
    // Without a pricing entry, characters are the conservative stand-in.
    assert!(!audiobook::elevenlabs_quota_fits(5_000, None, 4_999));
    assert!(audiobook::elevenlabs_quota_fits(5_000, None, 5_000));
}

/// A pdftohtml-shaped EPUB: one spine document per printed page, each
/// carrying the generator marker that turns on PDF cleanup and page grouping.
fn build_pdf_paged_fixture_epub(path: &Path, pages: usize) {
    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    let mut zip = ZipWriter::new(std::fs::File::create(path).unwrap());
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    zip.start_file("mimetype", stored).unwrap();
    zip.write_all(b"application/epub+zip").unwrap();
    zip.start_file("META-INF/container.xml", stored).unwrap();
    zip.write_all(FIXTURE_CONTAINER_XML.as_bytes()).unwrap();
    let mut manifest = String::new();
    let mut spine = String::new();
    for page in 1..=pages {
        let html = format!(
            r#"<html xmlns="http://www.w3.org/1999/xhtml"><head><title>Printed page</title><meta name="generator" content="pdftohtml"/></head><body><p>Sheet {page} describes a lighthouse keeper counting gulls along the breakwater.</p></body></html>"#
        );
        zip.start_file(format!("page{page}.xhtml"), stored).unwrap();
        zip.write_all(html.as_bytes()).unwrap();
        manifest.push_str(&format!(
            r#"<item id="p{page}" href="page{page}.xhtml" media-type="application/xhtml+xml"/>"#
        ));
        spine.push_str(&format!(r#"<itemref idref="p{page}"/>"#));
    }
    let opf = format!(
        r#"<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="uid">pdf-paged-fixture</dc:identifier><dc:title>Paged Fixture</dc:title><dc:language>en</dc:language></metadata><manifest>{manifest}</manifest><spine>{spine}</spine></package>"#
    );
    zip.start_file("content.opf", stored).unwrap();
    zip.write_all(opf.as_bytes()).unwrap();
    zip.finish().unwrap();
}

/// The estimate endpoint must group PDF pages exactly like narration does,
/// instead of quoting one chapter per printed page.
#[tokio::test]
async fn audiobook_estimate_groups_pdf_pages_like_narration() {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    const PAGES: usize = 8;
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("paged.epub");
    build_pdf_paged_fixture_epub(&input, PAGES);

    let scratch = tempfile::tempdir().unwrap();
    let narration = bookforge_audio::read_narration_source(&input, scratch.path()).unwrap();
    assert!(
        narration.pdf_page_grouping,
        "the generator marker must enable page grouping"
    );
    let expected = plan_audiobook_estimate(
        &input,
        scratch.path(),
        AudioProviderKind::Mock,
        "mock-silence",
        2_000,
        None,
    )
    .unwrap();
    assert!(
        expected.0 < PAGES,
        "grouping must not quote one chapter per page: {expected:?}"
    );

    let mut body = b"--B\r\nContent-Disposition: form-data; name=\"provider\"\r\n\r\nmock\r\n--B\r\nContent-Disposition: form-data; name=\"file\"; filename=\"paged.epub\"\r\nContent-Type: application/epub+zip\r\n\r\n".to_vec();
    body.extend_from_slice(&std::fs::read(&input).unwrap());
    body.extend_from_slice(b"\r\n--B--\r\n");
    let upload_dir = tempfile::tempdir().unwrap();
    let response = dashboard_router(test_state_with_upload_dir(
        "token-123",
        upload_dir.path().to_path_buf(),
    ))
    .oneshot(
        Request::builder()
            .method("POST")
            .uri("/api/audiobook/estimate")
            .header("host", TEST_HOST)
            .header("cookie", session_cookie_value("token-123"))
            .header("content-type", "multipart/form-data; boundary=B")
            .body(Body::from(body))
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let estimate = response_json(response).await;
    assert_eq!(estimate["chapters"], json!(expected.0));
    assert_eq!(estimate["chunks"], json!(expected.1));
    assert_eq!(estimate["characters"], json!(expected.2));
}

fn directory_names(path: &Path) -> Vec<String> {
    let mut names = std::fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    names
}

fn write_single_entry_zip(file: &mut std::fs::File, body: &[u8]) -> Result<()> {
    let mut archive = zip::ZipWriter::new(file);
    archive.start_file("audio.wav", zip::write::SimpleFileOptions::default())?;
    archive.write_all(body)?;
    archive.finish()?;
    Ok(())
}

fn zip_entry(path: &Path, name: &str) -> Result<Vec<u8>> {
    use std::io::Read as _;

    let mut archive = zip::ZipArchive::new(std::fs::File::open(path)?)?;
    let mut contents = Vec::new();
    archive.by_name(name)?.read_to_end(&mut contents)?;
    Ok(contents)
}

#[test]
fn failed_artifact_writer_preserves_prior_state_and_unrelated_stage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("process.json");
    let previous = br#"{"status":"running","warnings":["keep me"]}"#;
    std::fs::write(&path, previous).unwrap();
    // The fixed stage name older builds used; another writer may own it.
    let legacy = dir.path().join("process.part.tmp");
    std::fs::write(&legacy, b"another writer owns this").unwrap();

    let error = write_audio_artifact(dir.path(), "process.json", |file| {
        file.write_all(b"partial new state")?;
        anyhow::bail!("injected writer failure")
    })
    .unwrap_err();

    assert!(
        error.to_string().contains("injected writer failure"),
        "{error:#}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), previous);
    assert_eq!(std::fs::read(&legacy).unwrap(), b"another writer owns this");
    assert_eq!(
        directory_names(dir.path()),
        ["process.json", "process.part.tmp"]
    );
}

#[test]
fn concurrent_artifact_writers_publish_only_their_completed_zip() {
    use std::sync::mpsc;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(AUDIO_DOWNLOAD_ZIP_NAME);
    let wait = Duration::from_secs(30);

    // The first writer completes its archive and pauses before publishing.
    let (first_ready, first_written) = mpsc::channel();
    let (release_first, first_released) = mpsc::channel::<()>();
    let first_dir = dir.path().to_path_buf();
    let first = std::thread::spawn(move || {
        write_audio_artifact(&first_dir, AUDIO_DOWNLOAD_ZIP_NAME, |file| {
            write_single_entry_zip(file, b"first complete audio")?;
            first_ready.send(())?;
            first_released.recv_timeout(wait)?;
            Ok(())
        })
    });
    first_written.recv_timeout(wait).unwrap();

    // The second writer holds its own, still empty, stage open meanwhile.
    let (second_ready, second_opened) = mpsc::channel();
    let (release_second, second_released) = mpsc::channel::<()>();
    let second_dir = dir.path().to_path_buf();
    let second = std::thread::spawn(move || {
        write_audio_artifact(&second_dir, AUDIO_DOWNLOAD_ZIP_NAME, |file| {
            second_ready.send(())?;
            second_released.recv_timeout(wait)?;
            write_single_entry_zip(file, b"second complete audio")
        })
    });
    second_opened.recv_timeout(wait).unwrap();

    release_first.send(()).unwrap();
    first.join().unwrap().unwrap();
    assert_eq!(
        zip_entry(&path, "audio.wav").unwrap(),
        b"first complete audio",
        "the first writer publishes only its own completed archive"
    );
    release_second.send(()).unwrap();
    second.join().unwrap().unwrap();
    assert_eq!(
        zip_entry(&path, "audio.wav").unwrap(),
        b"second complete audio"
    );
    assert_eq!(directory_names(dir.path()), [AUDIO_DOWNLOAD_ZIP_NAME]);
}

const AUDIO_DOWNLOAD_ZIP_NAME: &str = "audiobook-audio.zip";

fn write_fixture_manifest(dir: &Path, parts: usize, status: &str) -> Vec<u8> {
    let chunks = (1..=parts)
        .map(|part| {
            let file = format!("part-{part}.wav");
            std::fs::write(dir.join(&file), format!("synthetic audio part {part}")).unwrap();
            json!({
                "chapter_index": 0,
                "chapter_title": "Chapter",
                "part": part,
                "kind": "body",
                "file": file,
                "chars": 10,
                "synthesis_sha256": "fixture",
                "status": "synthesized",
            })
        })
        .collect::<Vec<_>>();
    let bytes = serde_json::to_vec(&json!({
        "schema_version": 3,
        "title": "Fixture",
        "synthesis_id": "fixture",
        "voice": "voice",
        "format": "wav",
        "speed": 1.0,
        "max_chars": 2000,
        "chapters": 1,
        "chunks": chunks,
        "completed_chunks": parts,
        "status": status,
        "updated_at_ms": 0,
    }))
    .unwrap();
    std::fs::write(dir.join("manifest.json"), &bytes).unwrap();
    bytes
}

#[test]
fn download_zip_rebuilds_after_manifest_revision() {
    let dir = tempfile::tempdir().unwrap();
    let original = write_fixture_manifest(dir.path(), 1, "running");
    let path = ensure_audio_download_zip(dir.path()).unwrap();
    assert_eq!(zip_entry(&path, "manifest.json").unwrap(), original);
    assert!(zip_entry(&path, "part-2.wav").is_err());

    // Unchanged manifest: the cached archive is reused as is.
    let cached = std::fs::read(&path).unwrap();
    assert_eq!(ensure_audio_download_zip(dir.path()).unwrap(), path);
    assert_eq!(std::fs::read(&path).unwrap(), cached);

    // A later checkpoint added a part: the archive must be rebuilt.
    let current = write_fixture_manifest(dir.path(), 2, "succeeded");
    assert_eq!(ensure_audio_download_zip(dir.path()).unwrap(), path);
    assert_eq!(zip_entry(&path, "manifest.json").unwrap(), current);
    assert_eq!(
        zip_entry(&path, "part-1.wav").unwrap(),
        b"synthetic audio part 1"
    );
    assert_eq!(
        zip_entry(&path, "part-2.wav").unwrap(),
        b"synthetic audio part 2"
    );
    assert_eq!(
        directory_names(dir.path()),
        [
            AUDIO_DOWNLOAD_ZIP_NAME,
            "manifest.json",
            "part-1.wav",
            "part-2.wav"
        ]
    );
}

#[cfg(windows)]
#[test]
fn failed_process_state_replacement_preserves_previous_file() {
    use std::os::windows::fs::OpenOptionsExt;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("process.json");
    let previous = br#"{"status":"running","warnings":["keep me"]}"#;
    std::fs::write(&path, previous).unwrap();
    // Share read and write but not delete: the destination cannot be
    // replaced while this handle is open.
    let holder = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0x1 | 0x2)
        .open(&path)
        .unwrap();
    assert!(write_audio_process_state(dir.path(), "succeeded", None, None, false, None).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), previous);
    assert_eq!(directory_names(dir.path()), ["process.json"]);
    drop(holder);

    write_audio_process_state(dir.path(), "succeeded", None, None, false, None).unwrap();
    let process: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(process["status"], "succeeded");
    assert_eq!(process["warnings"], json!(["keep me"]));
    assert_eq!(directory_names(dir.path()), ["process.json"]);
}

/// Fixture body for the cancellation tests. A no-op in an ordinary test run;
/// when re-executed by [`cancel_fixture_child`] it stands in for an audiobook
/// child: `cooperate` cleans up and exits once the marker appears, `ignore`
/// never reacts to it.
#[test]
fn audio_cancel_process_fixture() {
    let Ok(mode) = std::env::var("BOOKFORGE_AUDIO_CANCEL_FIXTURE") else {
        return;
    };
    let marker = PathBuf::from(std::env::var_os("BOOKFORGE_AUDIO_CANCEL_MARKER").unwrap());
    let ready = PathBuf::from(std::env::var_os("BOOKFORGE_AUDIO_CANCEL_READY").unwrap());
    std::fs::write(&ready, b"ready").unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if mode == "cooperate" && marker.exists() {
            std::fs::write(ready.with_extension("cleaned"), b"cleanup complete").unwrap();
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

async fn cancel_fixture_child(mode: &str, marker: &Path, ready: &Path) -> tokio::process::Child {
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "commands::serve::tests::audio_dashboard::audio_cancel_process_fixture",
            "--nocapture",
        ])
        .env("BOOKFORGE_AUDIO_CANCEL_FIXTURE", mode)
        .env("BOOKFORGE_AUDIO_CANCEL_MARKER", marker)
        .env("BOOKFORGE_AUDIO_CANCEL_READY", ready)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn().unwrap();
    let started = tokio::time::timeout(Duration::from_secs(30), async {
        while !ready.exists() {
            assert!(
                child.try_wait().unwrap().is_none(),
                "the fixture exited before it was ready"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    if started.is_err() {
        let _ = child.kill().await;
        panic!("the cancellation fixture did not start");
    }
    child
}

#[tokio::test]
async fn active_audio_cancel_waits_for_cleanup_and_removes_marker() {
    let dir = tempfile::tempdir().unwrap();
    let marker = audio_cancel_marker(dir.path());
    let ready = dir.path().join("ready");
    let mut child = cancel_fixture_child("cooperate", &marker, &ready).await;

    cancel_running_audiobook(&mut child, &marker, Duration::from_secs(20))
        .await
        .unwrap();

    assert!(
        child.try_wait().unwrap().unwrap().success(),
        "a cooperative child exits on its own"
    );
    assert!(
        ready.with_extension("cleaned").exists(),
        "cleanup ran before exit"
    );
    assert!(!marker.exists(), "the marker must not outlive the cancel");
}

#[tokio::test]
async fn active_audio_cancel_force_stops_unresponsive_child_after_grace() {
    let dir = tempfile::tempdir().unwrap();
    let marker = audio_cancel_marker(dir.path());
    let ready = dir.path().join("ready");
    let mut child = cancel_fixture_child("ignore", &marker, &ready).await;

    let started = Instant::now();
    cancel_running_audiobook(&mut child, &marker, Duration::from_millis(50))
        .await
        .unwrap();

    assert!(
        started.elapsed() < Duration::from_secs(20),
        "an unresponsive child is force-stopped, not awaited: {:?}",
        started.elapsed()
    );
    assert!(
        child.try_wait().unwrap().is_some(),
        "the stopped child is reaped"
    );
    assert!(!ready.with_extension("cleaned").exists());
    assert!(!marker.exists(), "the marker must not outlive the cancel");
}
