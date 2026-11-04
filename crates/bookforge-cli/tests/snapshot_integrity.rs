//! A job's saved input EPUB is checked against its recorded SHA-256 before
//! `resume`, `review` or `correct` use it. All fixtures are synthetic.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

use assert_cmd::Command;
use bookforge_store::JobStore;
use serde_json::Value;
use tempfile::TempDir;
use zip::{ZipWriter, write::SimpleFileOptions};

struct Fixture {
    dir: TempDir,
    source: PathBuf,
    output: PathBuf,
    store: JobStore,
    job_id: String,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.epub");
        let output = dir.path().join("translated.epub");
        let events = dir.path().join("events.jsonl");
        build_epub(&source, "p { color: black; }");
        command(dir.path())
            .args(["translate", source.to_str().unwrap()])
            .args([
                "--provider",
                "mock",
                "--model",
                "mock-prefix-target",
                "--target",
                "Italian",
                "--qa",
                "off",
                "--ui",
                "quiet",
                "--out",
            ])
            .arg(&output)
            .arg("--progress-jsonl")
            .arg(&events)
            .assert()
            .success();
        let job_id = fs::read_to_string(events)
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find_map(|event| event["JobCreated"]["job_id"].as_str().map(str::to_owned))
            .unwrap();
        let store = JobStore::open(dir.path().join(".bookforge/jobs.sqlite")).unwrap();
        Self {
            dir,
            source,
            output,
            store,
            job_id,
        }
    }

    fn command(&self) -> Command {
        command(self.dir.path())
    }

    fn saved_input(&self) -> PathBuf {
        let snapshot = self
            .store
            .load_job_config_snapshot(&self.job_id)
            .unwrap()
            .unwrap();
        self.dir
            .path()
            .join(snapshot.input_snapshot_path.as_ref().unwrap())
    }
}

fn command(dir: &Path) -> Command {
    let mut command = Command::cargo_bin("bookforge").unwrap();
    command.current_dir(dir).timeout(Duration::from_secs(60));
    command
}

/// A one-chapter EPUB whose text and segment IDs stay the same when only the
/// stylesheet changes.
fn build_epub(path: &Path, css: &str) {
    let mut zip = ZipWriter::new(fs::File::create(path).unwrap());
    let entries = [
        ("mimetype", "application/epub+zip"),
        (
            "META-INF/container.xml",
            r#"<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#,
        ),
        (
            "content.opf",
            r#"<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="uid">snapshot-integrity</dc:identifier><dc:title>Integrity Fixture</dc:title><dc:language>en</dc:language></metadata><manifest><item id="chapter" href="chapter.xhtml" media-type="application/xhtml+xml"/><item id="style" href="style.css" media-type="text/css"/></manifest><spine><itemref idref="chapter"/></spine></package>"#,
        ),
        (
            "chapter.xhtml",
            r#"<html xmlns="http://www.w3.org/1999/xhtml"><head><title>One</title><link rel="stylesheet" href="style.css"/></head><body><p>The lighthouse keeper counted seven ships before the fog rolled in.</p></body></html>"#,
        ),
        ("style.css", css),
    ];
    for (name, contents) in entries {
        zip.start_file(name, SimpleFileOptions::default()).unwrap();
        zip.write_all(contents.as_bytes()).unwrap();
    }
    zip.finish().unwrap();
}

#[test]
fn snapshot_is_an_independent_copy_of_the_source() {
    let fixture = Fixture::new();
    let saved = fixture.saved_input();
    let before = fs::read(&saved).unwrap();
    build_epub(&fixture.source, "p { color: red; }");
    assert_eq!(
        fs::read(&saved).unwrap(),
        before,
        "editing the original book must not change the job's saved input"
    );
}

#[test]
fn changed_snapshot_stops_resume_review_and_correction_before_work() {
    let fixture = Fixture::new();
    let saved = fixture.saved_input();
    let original = fs::read(&saved).unwrap();
    let segment_id = fixture.store.segment_records(&fixture.job_id).unwrap()[0]
        .id
        .clone();
    // A valid EPUB with the same text and segment IDs but different assets.
    build_epub(&saved, "p { color: transparent; }");
    fixture
        .store
        .mark_segment_failed(&fixture.job_id, &segment_id, "fixture retry")
        .unwrap();
    fixture.store.mark_job_stopped(&fixture.job_id).unwrap();
    let blocks_before = fixture
        .store
        .load_block_translations(&fixture.job_id)
        .unwrap();
    let output_before = fs::read(&fixture.output).unwrap();
    let rejected_review = fixture.dir.path().join("rejected-review");

    fixture
        .command()
        .args(["review", &fixture.job_id, "--out"])
        .arg(&rejected_review)
        .assert()
        .failure()
        .stderr(predicates::str::contains("input snapshot SHA-256 mismatch"));
    fixture
        .command()
        .args([
            "correct",
            &fixture.job_id,
            "--segment",
            &segment_id,
            "--text",
            "Correzione.",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("input snapshot SHA-256 mismatch"));
    fixture
        .command()
        .args(["resume", &fixture.job_id, "--ui", "quiet"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("input snapshot SHA-256 mismatch"));

    assert!(!rejected_review.exists());
    assert_eq!(fs::read(&fixture.output).unwrap(), output_before);
    assert_eq!(
        fixture
            .store
            .load_block_translations(&fixture.job_id)
            .unwrap(),
        blocks_before
    );
    assert_eq!(
        fixture
            .store
            .get_job(&fixture.job_id)
            .unwrap()
            .unwrap()
            .status,
        "stopped"
    );

    // Restoring the exact saved input makes the checkpoints usable again.
    fs::write(&saved, original).unwrap();
    fixture
        .command()
        .args(["resume", &fixture.job_id, "--ui", "quiet"])
        .assert()
        .success();
}

#[test]
fn job_row_hash_is_checked_when_the_run_snapshot_has_none() {
    let fixture = Fixture::new();
    let mut snapshot = fixture
        .store
        .load_job_config_snapshot(&fixture.job_id)
        .unwrap()
        .unwrap();
    let saved = fixture.saved_input();
    let hash = snapshot.input_sha256.take().unwrap();
    fixture
        .store
        .update_job_config_snapshot(&fixture.job_id, &snapshot)
        .unwrap();
    fixture
        .store
        .update_job_input_snapshot(&fixture.job_id, &saved, &hash)
        .unwrap();
    assert!(
        fixture
            .store
            .get_job(&fixture.job_id)
            .unwrap()
            .unwrap()
            .input_sha256
            .is_some()
    );
    fixture
        .command()
        .args(["review", &fixture.job_id])
        .assert()
        .success();
    build_epub(&saved, "p { color: transparent; }");
    fixture
        .command()
        .args(["review", &fixture.job_id])
        .assert()
        .failure()
        .stderr(predicates::str::contains("input snapshot SHA-256 mismatch"));
}
