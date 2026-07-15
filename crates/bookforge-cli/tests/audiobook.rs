//! End-to-end coverage for `bookforge audiobook` over a synthetic EPUB,
//! using the offline mock TTS provider so the test needs no network and is
//! deterministic in CI.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use assert_cmd::Command;
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

fn bookforge() -> Command {
    Command::cargo_bin("bookforge").expect("bookforge binary should be built")
}

const CONTAINER_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;

const OPF: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="uid">audiobook-fixture</dc:identifier>
    <dc:title>Audiobook Fixture</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="ch1" href="chapter1.xhtml" media-type="application/xhtml+xml"/>
    <item id="ch2" href="chapter2.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="ch1"/>
    <itemref idref="ch2"/>
  </spine>
</package>"#;

const CHAPTER_ONE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<head><title>Chapter One</title></head>
<body>
<h1>The First Chapter</h1>
<p>This is the first paragraph. It has a couple of sentences to narrate.</p>
<p>Here is a second paragraph with a little more text so that chunking has something to work with.</p>
</body>
</html>"#;

const CHAPTER_TWO: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<head><title>Chapter Two</title></head>
<body>
<h1>The Second Chapter</h1>
<p>The second chapter also has prose. Every sentence here should reach the narrator.</p>
</body>
</html>"#;

fn build_fixture(path: &Path) {
    let file = fs::File::create(path).expect("fixture EPUB should be creatable");
    let mut zip = ZipWriter::new(file);
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    zip.start_file("mimetype", stored).unwrap();
    zip.write_all(b"application/epub+zip").unwrap();
    zip.start_file("META-INF/container.xml", deflated).unwrap();
    zip.write_all(CONTAINER_XML.as_bytes()).unwrap();
    zip.start_file("content.opf", deflated).unwrap();
    zip.write_all(OPF.as_bytes()).unwrap();
    zip.start_file("chapter1.xhtml", deflated).unwrap();
    zip.write_all(CHAPTER_ONE.as_bytes()).unwrap();
    zip.start_file("chapter2.xhtml", deflated).unwrap();
    zip.write_all(CHAPTER_TWO.as_bytes()).unwrap();
    zip.finish().unwrap();
}

fn fixture(dir: &Path) -> PathBuf {
    let path = dir.join("book.epub");
    build_fixture(&path);
    path
}

#[test]
fn audiobook_mock_writes_files_and_manifest() {
    let temp = tempfile::tempdir().expect("temp dir");
    let input = fixture(temp.path());
    let out = temp.path().join("audio");

    bookforge()
        .current_dir(temp.path())
        .args([
            "audiobook",
            input.to_str().unwrap(),
            "--provider",
            "mock",
            "--out",
            out.to_str().unwrap(),
            "--format",
            "wav",
        ])
        .assert()
        .success();

    let manifest = out.join("manifest.json");
    assert!(manifest.exists(), "manifest should be written");
    let files: Vec<_> = fs::read_dir(&out)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "wav"))
        .collect();
    assert!(
        !files.is_empty(),
        "at least one audio file should be written"
    );
    let first_audio = fs::read(files[0].path()).expect("audio should be readable");
    assert_eq!(&first_audio[..4], b"RIFF", "mock output must be real WAV");

    let manifest_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&manifest).unwrap()).unwrap();
    assert_eq!(manifest_json["schema_version"], 2);
    assert_eq!(manifest_json["status"], "succeeded");
    assert_eq!(
        manifest_json["completed_chunks"],
        manifest_json["chunks"].as_array().unwrap().len()
    );
    assert_eq!(manifest_json["synthesis_id"], "mock:mock-silence");
    assert_eq!(manifest_json["chapters"], 2);
    let chunks = manifest_json["chunks"].as_array().unwrap();
    assert!(chunks.len() >= 2);
    assert_eq!(chunks[0]["synthesis_sha256"].as_str().unwrap().len(), 64);
}

#[test]
fn audiobook_dry_run_writes_nothing() {
    let temp = tempfile::tempdir().expect("temp dir");
    let input = fixture(temp.path());
    let out = temp.path().join("audio-dry");

    let assert = bookforge()
        .current_dir(temp.path())
        .args([
            "audiobook",
            input.to_str().unwrap(),
            "--provider",
            "mock",
            "--out",
            out.to_str().unwrap(),
            "--dry-run",
        ])
        .assert()
        .success();

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    assert!(
        stdout.contains("Dry run"),
        "should announce dry run: {stdout}"
    );
    assert!(!out.exists(), "dry run must not create the output dir");
}

#[test]
fn audiobook_resume_skips_existing_files() {
    let temp = tempfile::tempdir().expect("temp dir");
    let input = fixture(temp.path());
    let out = temp.path().join("audio-resume");

    let run = || {
        bookforge()
            .current_dir(temp.path())
            .args([
                "audiobook",
                input.to_str().unwrap(),
                "--provider",
                "mock",
                "--out",
                out.to_str().unwrap(),
            ])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    };

    run();
    let second_stdout = run();
    let second = String::from_utf8_lossy(&second_stdout);
    assert!(
        second.contains("0 synthesized"),
        "second run should reuse everything: {second}"
    );
}

#[test]
fn audiobook_changed_voice_does_not_reuse_stale_chunks() {
    let temp = tempfile::tempdir().expect("temp dir");
    let input = fixture(temp.path());
    let out = temp.path().join("audio-identity");

    for voice in ["voice-a", "voice-b"] {
        let assert = bookforge()
            .current_dir(temp.path())
            .args([
                "audiobook",
                input.to_str().unwrap(),
                "--provider",
                "mock",
                "--voice",
                voice,
                "--out",
                out.to_str().unwrap(),
            ])
            .assert()
            .success();
        let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
        assert!(
            !stdout.contains("0 synthesized"),
            "changed synthesis settings must produce new chunks: {stdout}"
        );
    }

    let audio_count = fs::read_dir(&out)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "wav"))
        .count();
    assert!(
        audio_count >= 4,
        "both hashed generations should remain auditable"
    );
}

#[test]
fn audiobook_mock_rejects_mislabeled_format() {
    let temp = tempfile::tempdir().expect("temp dir");
    let input = fixture(temp.path());

    let assert = bookforge()
        .current_dir(temp.path())
        .args([
            "audiobook",
            input.to_str().unwrap(),
            "--provider",
            "mock",
            "--format",
            "mp3",
        ])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(stderr.contains("mock provider emits WAV audio"));
}

#[test]
fn audiobook_gemini_dry_run_uses_provider_defaults_without_a_key() {
    let temp = tempfile::tempdir().expect("temp dir");
    let input = fixture(temp.path());

    let assert = bookforge()
        .current_dir(temp.path())
        .args([
            "audiobook",
            input.to_str().unwrap(),
            "--provider",
            "gemini",
            "--dry-run",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    assert!(stdout.contains("Voice: Kore | Format: wav"));
    assert!(stdout.contains("Model: gemini-3.1-flash-tts-preview"));
}

#[test]
fn audiobook_gemini_rejects_mislabeled_compressed_format() {
    let temp = tempfile::tempdir().expect("temp dir");
    let input = fixture(temp.path());

    let assert = bookforge()
        .current_dir(temp.path())
        .args([
            "audiobook",
            input.to_str().unwrap(),
            "--provider",
            "gemini",
            "--format",
            "mp3",
            "--dry-run",
        ])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(stderr.contains("Gemini TTS returns 24 kHz PCM"));
}

#[test]
fn audiobook_elevenlabs_requires_voice_id() {
    let temp = tempfile::tempdir().expect("temp dir");
    let input = fixture(temp.path());

    let assert = bookforge()
        .current_dir(temp.path())
        .args([
            "audiobook",
            input.to_str().unwrap(),
            "--provider",
            "elevenlabs",
            "--dry-run",
        ])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(stderr.contains("requires --voice"));
}

#[test]
fn audiobook_elevenlabs_dry_run_accepts_native_provider_options() {
    let temp = tempfile::tempdir().expect("temp dir");
    let input = fixture(temp.path());

    let assert = bookforge()
        .current_dir(temp.path())
        .args([
            "audiobook",
            input.to_str().unwrap(),
            "--provider",
            "elevenlabs",
            "--voice",
            "JBFqnCBsd6RMkjVDRZzb",
            "--dry-run",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    assert!(stdout.contains("Voice: JBFqnCBsd6RMkjVDRZzb | Format: mp3"));
    assert!(stdout.contains("Model: eleven_multilingual_v2"));
}

#[test]
fn audiobook_json_output_remains_json_through_stitching() {
    let temp = tempfile::tempdir().expect("temp dir");
    let input = fixture(temp.path());
    let out = temp.path().join("audio-json");
    let assert = bookforge()
        .current_dir(temp.path())
        .args([
            "audiobook",
            input.to_str().unwrap(),
            "--provider",
            "mock",
            "--out",
            out.to_str().unwrap(),
            "--stitch",
            "--ui",
            "json",
        ])
        .assert()
        .success();

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    let events = stdout
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("JSON event line"))
        .collect::<Vec<_>>();
    assert!(!events.is_empty());
    assert_eq!(events.last().unwrap()["event"], "audiobook_finished");
    assert_eq!(events.last().unwrap()["status"], "succeeded");
}

#[test]
fn audiobook_quiet_output_stays_silent_through_stitching() {
    let temp = tempfile::tempdir().expect("temp dir");
    let input = fixture(temp.path());
    let out = temp.path().join("audio-quiet");
    let assert = bookforge()
        .current_dir(temp.path())
        .args([
            "audiobook",
            input.to_str().unwrap(),
            "--provider",
            "mock",
            "--out",
            out.to_str().unwrap(),
            "--stitch",
            "--ui",
            "quiet",
        ])
        .assert()
        .success();

    assert!(assert.get_output().stdout.is_empty());
    assert!(assert.get_output().stderr.is_empty());
}

fn chunk_files(out: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(out)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        // Managed chunk files carry the `-part-` segment; stitched outputs and
        // the manifest do not.
        .filter(|name| name.contains("-part-"))
        .collect();
    names.sort();
    names
}

#[test]
fn audiobook_prune_removes_only_stale_chunks_from_earlier_runs() {
    let temp = tempfile::tempdir().expect("temp dir");
    let input = fixture(temp.path());
    let out = temp.path().join("audio-prune");

    let run = |voice: &str, prune: bool| {
        let mut args = vec![
            "audiobook".to_string(),
            input.to_str().unwrap().to_string(),
            "--provider".to_string(),
            "mock".to_string(),
            "--out".to_string(),
            out.to_str().unwrap().to_string(),
            "--voice".to_string(),
            voice.to_string(),
        ];
        if prune {
            args.push("--prune".to_string());
        }
        bookforge()
            .current_dir(temp.path())
            .args(&args)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    };

    // First run writes chunk files hashed with the "alloy" voice.
    run("alloy", false);
    let first = chunk_files(&out);
    assert!(!first.is_empty(), "first run should write chunk files");

    // Second run uses a different voice, so it produces new file names and
    // leaves the "alloy" files orphaned. --prune should delete exactly those.
    let stdout = run("nova", true);
    let stdout = String::from_utf8_lossy(&stdout);
    assert!(
        stdout.contains("Prune: removed"),
        "prune should report removals: {stdout}"
    );

    let remaining = chunk_files(&out);
    assert!(!remaining.is_empty(), "current run's chunks must survive");
    for name in &first {
        assert!(
            !remaining.contains(name),
            "stale chunk {name} should have been pruned"
        );
    }
    // A second prune run with the same voice finds nothing to remove.
    let stdout = run("nova", true);
    let stdout = String::from_utf8_lossy(&stdout);
    assert!(
        stdout.contains("no stale chunks"),
        "idempotent prune should report nothing to remove: {stdout}"
    );
}
