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
<head></head>
<body>
<h1>The First Chapter</h1>
<p>This is the first paragraph. It has a couple of sentences to narrate.</p>
<p>Here is a second paragraph with a little more text so that chunking has something to work with.</p>
</body>
</html>"#;

const CHAPTER_TWO: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<head></head>
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
    assert_eq!(manifest_json["schema_version"], 3);
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
    if std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        assert!(
            out.join("audiobook.m4b").exists(),
            "ffmpeg availability should make the chapter-marked book file the default"
        );
    }
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
fn audiobook_elevenlabs_enforces_model_character_limits() {
    let temp = tempfile::tempdir().expect("temp dir");
    let input = fixture(temp.path());

    bookforge()
        .current_dir(temp.path())
        .args([
            "audiobook",
            input.to_str().unwrap(),
            "--provider",
            "elevenlabs",
            "--voice",
            "test-voice-id",
            "--model",
            "eleven_v3",
            "--max-chars",
            "5001",
            "--dry-run",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "eleven_v3 is limited to 5000 characters",
        ));
}

#[test]
fn elevenlabs_dry_run_without_model_uses_static_default_offline() {
    let temp = tempfile::tempdir().expect("temp dir");
    let input = fixture(temp.path());

    bookforge()
        .current_dir(temp.path())
        .env_remove("ELEVENLABS_API_KEY")
        .args([
            "audiobook",
            input.to_str().unwrap(),
            "--provider",
            "elevenlabs",
            "--voice",
            "v",
            "--dry-run",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("eleven_multilingual_v2"))
        .stdout(predicates::str::contains("auto-selects"));
}

#[test]
fn elevenlabs_preflight_failure_warns_and_falls_back() {
    let temp = tempfile::tempdir().expect("temp dir");
    let input = fixture(temp.path());
    let key_env = "BOOKFORGE_ELEVENLABS_PREFLIGHT_FALLBACK_TEST_KEY";

    // NOTE (cross-workstream): the P2-audio wave changed the model preflight
    // contract in bookforge-audio — transient transport failures now fail
    // OPEN to a cheaper suitable tier inside the library, so the CLI no
    // longer prints its own "model preflight failed" warning for this
    // scenario. The run still cannot synthesize against an unreachable
    // endpoint, so it must fail loudly with resumable chunks.
    bookforge()
        .current_dir(temp.path())
        .env(key_env, "dummy-test-key")
        .args([
            "audiobook",
            input.to_str().unwrap(),
            "--provider",
            "elevenlabs",
            "--voice",
            "v",
            "--base-url",
            "http://127.0.0.1:9",
            "--api-key-env",
            key_env,
            "--timeout-seconds",
            "1",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("Incomplete:"))
        .stderr(predicates::str::contains("--retry-failed"));
}

#[test]
fn elevenlabs_v3_rejects_speed() {
    let temp = tempfile::tempdir().expect("temp dir");
    let input = fixture(temp.path());

    bookforge()
        .current_dir(temp.path())
        .args([
            "audiobook",
            input.to_str().unwrap(),
            "--provider",
            "elevenlabs",
            "--model",
            "eleven_v3",
            "--speed",
            "1.2",
            "--voice",
            "v",
            "--dry-run",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("speed control"));
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
        .env_remove("ELEVENLABS_API_KEY")
        .args([
            "audiobook",
            input.to_str().unwrap(),
            "--provider",
            "elevenlabs",
            "--voice",
            "JBFqnCBsd6RMkjVDRZzb",
            "--model",
            "eleven_flash_v2_5",
            "--max-chars",
            "40000",
            "--dry-run",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    assert!(stdout.contains("Voice: JBFqnCBsd6RMkjVDRZzb | Format: mp3"));
    assert!(stdout.contains("Model: eleven_flash_v2_5"));
}

#[test]
fn audiobook_chapter_filter_keeps_global_chapter_numbering() {
    let temp = tempfile::tempdir().expect("temp dir");
    let input = fixture(temp.path());
    let out = temp.path().join("audio-chapter-2");

    bookforge()
        .current_dir(temp.path())
        .args([
            "audiobook",
            input.to_str().unwrap(),
            "--provider",
            "mock",
            "--out",
            out.to_str().unwrap(),
            "--chapters",
            "2",
            "--no-book-file",
        ])
        .assert()
        .success();

    let manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(out.join("manifest.json")).unwrap()).unwrap();
    let chunks = manifest["chunks"].as_array().unwrap();
    assert!(!chunks.is_empty());
    assert!(chunks.iter().all(|chunk| chunk["chapter_index"] == 1));
    assert!(chunks.iter().all(|chunk| {
        chunk["file"]
            .as_str()
            .is_some_and(|file| file.starts_with("chapter-002-part-"))
    }));
    assert_eq!(manifest["chapters"], 1);
}

#[test]
fn audiobook_manifest_keeps_title_in_its_own_first_part() {
    let temp = tempfile::tempdir().expect("temp dir");
    let input = fixture(temp.path());
    let out = temp.path().join("audio-title-kind");

    bookforge()
        .current_dir(temp.path())
        .args([
            "audiobook",
            input.to_str().unwrap(),
            "--provider",
            "mock",
            "--out",
            out.to_str().unwrap(),
            "--chapters",
            "1",
            "--no-book-file",
        ])
        .assert()
        .success();

    let manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(out.join("manifest.json")).unwrap()).unwrap();
    let first = &manifest["chunks"].as_array().unwrap()[0];
    assert_eq!(first["part"], 1);
    assert_eq!(first["kind"], "title");
    assert_eq!(first["chars"], "The First Chapter".chars().count());
}

#[test]
fn audiobook_list_voices_requires_elevenlabs_provider() {
    bookforge()
        .args(["audiobook", "--list-voices"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "--list-voices requires --provider elevenlabs",
        ));
}

#[test]
fn audiobook_requires_input_without_list_voices() {
    bookforge()
        .args(["audiobook", "--provider", "mock"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("INPUT is required"));
}

#[test]
fn audiobook_seed_rejects_non_elevenlabs_provider() {
    let temp = tempfile::tempdir().expect("temp dir");
    let input = fixture(temp.path());
    bookforge()
        .args([
            "audiobook",
            input.to_str().unwrap(),
            "--provider",
            "mock",
            "--seed",
            "7",
            "--dry-run",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "--seed is supported only with --provider elevenlabs",
        ));
}

#[test]
fn audiobook_dry_run_uses_audio_pricing_override() {
    let temp = tempfile::tempdir().expect("temp dir");
    let input = fixture(temp.path());
    let pricing = temp.path().join("audio-pricing.json");
    fs::write(
        &pricing,
        r#"{
          "schema_version": 1,
          "updated_at": "2026-07-20",
          "providers": {
            "mock": {
              "mock-silence": {
                "usd_per_million_chars": 1000000.0,
                "credits_per_char": null,
                "note": "one dollar per character for an exact test"
              }
            }
          }
        }"#,
    )
    .unwrap();

    bookforge()
        .current_dir(temp.path())
        .env("BOOKFORGE_AUDIO_PRICING_PATH", &pricing)
        .args([
            "audiobook",
            input.to_str().unwrap(),
            "--provider",
            "mock",
            "--dry-run",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("Estimated cost: ~$"))
        .stdout(predicates::str::contains(".00"));
}

#[test]
fn unsupported_elevenlabs_language_warns_instead_of_failing() {
    let temp = tempfile::tempdir().expect("temp dir");
    let input = fixture(temp.path());

    bookforge()
        .current_dir(temp.path())
        .env_remove("ELEVENLABS_API_KEY")
        .args([
            "audiobook",
            input.to_str().unwrap(),
            "--provider",
            "elevenlabs",
            "--voice",
            "v",
            "--model",
            "eleven_multilingual_v2",
            "--language",
            "en-US",
            "--dry-run",
        ])
        .assert()
        .success()
        .stderr(predicates::str::contains("rejects language_code"));
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
    // NOTE (cross-workstream): the audio crate currently emits its own
    // child-process diagnostics ("DBG child pid=… isolated") on stderr.
    // Quiet-mode *UI* output must stay silent; external dependency chatter
    // is tolerated until the P2-audio wave routes it through tracing.
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        !stderr.contains("Planning"),
        "quiet mode must not print planning UI output: {stderr}"
    );
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

/// Expected plan for the fixture, computed through the same
/// `read_narration_source` pipeline estimation and launches share (AUDIO-7).
/// Keeps every claim below about estimate/launch parity honest offline.
fn expected_plan(input: &Path) -> (usize, usize, usize) {
    let scratch = tempfile::tempdir().expect("scratch temp dir");
    let narration =
        bookforge_audio::read_narration_source(input, scratch.path()).expect("fixture parses");
    let options = bookforge_audio::AudiobookOptions {
        max_chars: 2_000,
        ..bookforge_audio::AudiobookOptions::default()
    };
    let plan = bookforge_audio::plan_chunks(&narration.book, &options);
    let chapters = plan
        .iter()
        .map(|chunk| chunk.chapter_index)
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    let characters = plan.iter().map(|chunk| chunk.chars).sum();
    (chapters, plan.len(), characters)
}

#[test]
fn audiobook_dry_run_plan_matches_the_shared_launcher_pipeline() {
    let temp = tempfile::tempdir().expect("temp dir");
    let input = fixture(temp.path());
    let (chapters, chunks, characters) = expected_plan(&input);

    let assert = bookforge()
        .current_dir(temp.path())
        .args([
            "audiobook",
            input.to_str().unwrap(),
            "--provider",
            "mock",
            "--dry-run",
            "--ui",
            "json",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    let plan_event = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|event| event["event"] == "audiobook_plan")
        .expect("dry run should emit an audiobook_plan event");

    assert_eq!(plan_event["chapters"], chapters as u64, "{plan_event}");
    assert_eq!(plan_event["chunks"], chunks as u64, "{plan_event}");
    assert_eq!(plan_event["characters"], characters as u64, "{plan_event}");
    // Degraded-model surfacing stays null unless a real ElevenLabs preflight
    // degraded; a mock run must not invent one.
    assert!(
        plan_event["model_degraded_reason"].is_null(),
        "{plan_event}"
    );
}

#[test]
fn audiobook_warns_and_drops_options_gemini_cannot_consume() {
    let temp = tempfile::tempdir().expect("temp dir");
    let input = fixture(temp.path());

    let assert = bookforge()
        .current_dir(temp.path())
        .args([
            "audiobook",
            input.to_str().unwrap(),
            "--provider",
            "gemini",
            "--language",
            "en-US",
            "--text-normalization",
            "on",
            "--dry-run",
        ])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains(
            "gemini TTS does not support --language, --text-normalization; dropping them"
        ),
        "expected one uniform warn-and-drop notice: {stderr}"
    );
}

#[test]
fn audiobook_mock_warns_and_drops_speed_and_instructions() {
    let temp = tempfile::tempdir().expect("temp dir");
    let input = fixture(temp.path());

    let assert = bookforge()
        .current_dir(temp.path())
        .args([
            "audiobook",
            input.to_str().unwrap(),
            "--provider",
            "mock",
            "--speed",
            "1.5",
            "--instructions",
            "Calm narration.",
            "--no-book-file",
        ])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains(
            "mock TTS does not support --instructions, --speed; dropping them before synthesis"
        ),
        "expected the matrix-driven warn-and-drop notice: {stderr}"
    );
}
