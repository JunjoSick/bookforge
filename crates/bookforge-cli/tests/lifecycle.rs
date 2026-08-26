use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process, thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use assert_cmd::Command;
use bookforge_core::{
    ControlCommand, GlossaryCategory, GlossaryStatus, GlossaryTerm, read_control_file,
    write_control_file,
};
use bookforge_store::{GlossaryFilter, JobStore, NewGlossaryCandidate, StoreError};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

fn bookforge() -> Command {
    Command::cargo_bin("bookforge").expect("bookforge binary should be built")
}

// Builds a synthetic EPUB so lifecycle tests run in CI and contributor
// checkouts without relying on local, gitignored book fixtures.
fn fixture_input() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "bookforge-lifecycle-fixture-{}-{nanos}.epub",
        std::process::id()
    ));
    build_lifecycle_epub(&path);
    path
}

fn build_lifecycle_epub(path: &Path) {
    let file = fs::File::create(path).expect("fixture EPUB should be creatable");
    let mut zip = ZipWriter::new(file);
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    zip.start_file("mimetype", stored).unwrap();
    zip.write_all(b"application/epub+zip").unwrap();
    zip.start_file("META-INF/container.xml", deflated).unwrap();
    zip.write_all(LIFECYCLE_CONTAINER_XML.as_bytes()).unwrap();
    zip.start_file("content.opf", deflated).unwrap();
    zip.write_all(LIFECYCLE_OPF.as_bytes()).unwrap();
    zip.start_file("nav.xhtml", deflated).unwrap();
    zip.write_all(LIFECYCLE_NAV.as_bytes()).unwrap();
    zip.start_file("chapter1.xhtml", deflated).unwrap();
    zip.write_all(LIFECYCLE_CHAPTER_ONE.as_bytes()).unwrap();
    zip.start_file("chapter2.xhtml", deflated).unwrap();
    zip.write_all(LIFECYCLE_CHAPTER_TWO.as_bytes()).unwrap();
    zip.finish().unwrap();
}

const LIFECYCLE_CONTAINER_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;

const LIFECYCLE_OPF: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="uid">lifecycle-fixture</dc:identifier>
    <dc:title>Lifecycle Fixture</dc:title>
    <dc:language>en</dc:language>
    <meta property="dcterms:modified">2026-01-01T00:00:00Z</meta>
  </metadata>
  <manifest>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
    <item id="ch1" href="chapter1.xhtml" media-type="application/xhtml+xml"/>
    <item id="ch2" href="chapter2.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="ch1"/>
    <itemref idref="ch2"/>
  </spine>
</package>"#;

const LIFECYCLE_NAV: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<head><title>Lifecycle Fixture Navigation</title></head>
<body>
<nav epub:type="toc" id="toc">
<h1>Table of contents</h1>
<ol>
<li><a href="chapter1.xhtml">Lifecycle Chapter One</a></li>
<li><a href="chapter2.xhtml">Lifecycle Chapter Two</a></li>
</ol>
</nav>
</body>
</html>"#;

const LIFECYCLE_CHAPTER_ONE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<head><title>Lifecycle Chapter One</title></head>
<body>
<h1>Lifecycle Chapter One</h1>
<p>Ivan Ilych met Peter Ivanovich near Mount Doom. Ivan Ilych carried the Ring while Aragorn watched Mount Doom.</p>
<p>Galadriel named Ivan Ilych and Ivan Ilych again. Mount Doom appeared, and Mount Doom remained in sight.</p>
<p>Hello <em>formatted</em> link text with <a href="https://example.com">Example Link</a>.</p>
</body>
</html>"#;

const LIFECYCLE_CHAPTER_TWO: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<head><title>Lifecycle Chapter Two</title></head>
<body>
<h1>Lifecycle Chapter Two</h1>
<p>Aragorn and Gandalf returned to Mount Doom. Ivan Ilych remembered Peter Ivanovich and Galadriel.</p>
<p>The Shire, Mount Doom, and Ivan Ilych are repeated here so glossary extraction has stable candidates.</p>
</body>
</html>"#;

#[test]
fn cli_translate_mock_quiet_writes_output_report_and_events() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let run = translate_quiet(&temp, "mock-prefix-target");

    assert!(run.output.exists(), "translated EPUB should exist");
    assert!(run.events.exists(), "event log should exist");
    assert!(run.report.exists(), "markdown report should exist");
}

#[test]
fn cli_italian_to_toki_pona_activates_built_in_translation_contract() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let input = fixture_input();
    let output = temp.path().join("lipu.epub");
    let events = temp.path().join("events.jsonl");

    bookforge()
        .current_dir(temp.path())
        .args([
            "translate",
            input.to_str().unwrap(),
            "--source",
            "Italian",
            "--target",
            "Toki Pona",
            "--provider",
            "mock",
            "--model",
            "mock-prefix-target",
            "--ui",
            "quiet",
            "--progress-jsonl",
            events.to_str().unwrap(),
            "--out",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();

    assert!(output.exists());
    let job_id = job_id_from_events(&events);
    let store = JobStore::open(temp.path().join(".bookforge/jobs.sqlite")).expect("store opens");
    let snapshot = store
        .load_job_config_snapshot(&job_id)
        .expect("snapshot loads")
        .expect("snapshot exists");
    assert_eq!(snapshot.source_language.as_deref(), Some("Italian"));
    assert_eq!(snapshot.target_language, "Toki Pona");
    assert!(snapshot.style_rendered_block.contains("Toki Pona grammar"));
    assert!(
        snapshot
            .style_rendered_block
            .contains("Do not soften, endorse, rebut")
    );
}

#[test]
fn cli_translate_unsupported_provider_exits_failure() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let input = fixture_input();
    let assert = bookforge()
        .current_dir(temp.path())
        .args([
            "translate",
            input.to_str().unwrap(),
            "--target",
            "Italian",
            "--provider",
            "not-a-provider",
            "--ui",
            "quiet",
        ])
        .assert()
        .failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains("unsupported translation provider 'not-a-provider'"),
        "stderr should explain unsupported provider, got: {stderr}"
    );
}

#[cfg(unix)]
#[test]
fn cli_translate_strict_validation_failure_updates_report_status() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let input = fixture_input();
    let output = temp.path().join("out.epub");
    let fake_epubcheck = temp.path().join("fake-epubcheck.sh");
    fs::write(
        &fake_epubcheck,
        r#"#!/usr/bin/env sh
report=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--json" ]; then
    shift
    report="$1"
  fi
  shift
done
cat > "$report" <<'JSON'
{"checker":{"checkerVersion":"test","nFatal":0,"nError":0,"nWarning":1},"messages":[{"severity":"WARNING","ID":"TEST","message":"strict warning"}]}
JSON
exit 0
"#,
    )
    .expect("fake epubcheck script should be writable");
    let mut permissions = fs::metadata(&fake_epubcheck)
        .expect("fake epubcheck metadata")
        .permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    fs::set_permissions(&fake_epubcheck, permissions).expect("fake epubcheck should be executable");

    bookforge()
        .current_dir(temp.path())
        .env("BOOKFORGE_EPUBCHECK", &fake_epubcheck)
        .args([
            "translate",
            input.to_str().unwrap(),
            "--target",
            "Italian",
            "--provider",
            "mock",
            "--model",
            "mock-prefix-target",
            "--strict-epubcheck",
            "--ui",
            "quiet",
            "--out",
            output.to_str().unwrap(),
        ])
        .assert()
        .failure();

    let report_path = temp.path().join("out.report.json");
    let report: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&report_path).expect("QA report should be written"),
    )
    .expect("QA report should be JSON");
    assert_eq!(report["status"], "failed");
    assert!(
        temp.path().join("out.validation.json").exists(),
        "validation report should be written"
    );
}

#[test]
fn cli_style_clear_book_scope_requires_scope_id() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let assert = bookforge()
        .current_dir(temp.path())
        .args(["style", "clear", "--scope", "book"])
        .assert()
        .failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains("requires a non-empty scope.id"),
        "stderr should explain missing style scope id, got: {stderr}"
    );
}

#[test]
fn cli_entities_clear_book_scope_requires_scope_id() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let assert = bookforge()
        .current_dir(temp.path())
        .args(["entities", "clear", "--scope", "book"])
        .assert()
        .failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains("requires a non-empty scope.id"),
        "stderr should explain missing entity scope id, got: {stderr}"
    );
}

#[test]
fn cli_translate_context_window_persists_snapshot_settings() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let input = fixture_input();
    let output = temp.path().join("out.epub");
    let events = temp.path().join("events.jsonl");
    bookforge()
        .current_dir(temp.path())
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
            "--context-window",
            "5",
            "--context-budget-tokens",
            "900",
            "--context-scope",
            "book",
            "--ui",
            "quiet",
            "--progress-jsonl",
            events.to_str().unwrap(),
            "--out",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();
    let job_id = job_id_from_events(&events);
    let store =
        JobStore::open(temp.path().join(".bookforge/jobs.sqlite")).expect("store should open");
    let snapshot = store
        .load_job_config_snapshot(&job_id)
        .expect("snapshot load")
        .expect("snapshot present");
    assert_eq!(snapshot.context_window, 5);
    assert_eq!(snapshot.context_budget_tokens, 900);
    assert_eq!(
        snapshot.context_scope,
        bookforge_core::config::ContextScope::Book
    );
}

#[test]
fn cli_translate_bilingual_options_persist_in_snapshot() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let input = fixture_input();
    let output = temp.path().join("out.epub");
    let events = temp.path().join("events.jsonl");
    let css_path = temp.path().join("bilingual.css");
    fs::write(&css_path, ".bookforge-translation { color: #123456; }\n")
        .expect("custom bilingual CSS should write");

    bookforge()
        .current_dir(temp.path())
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
            "--mode",
            "append-text",
            "--bilingual-separator",
            " -- ",
            "--bilingual-style",
            "prominent",
            "--bilingual-css",
            css_path.to_str().unwrap(),
            "--ui",
            "quiet",
            "--progress-jsonl",
            events.to_str().unwrap(),
            "--out",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();

    let job_id = job_id_from_events(&events);
    let store =
        JobStore::open(temp.path().join(".bookforge/jobs.sqlite")).expect("store should open");
    let snapshot = store
        .load_job_config_snapshot(&job_id)
        .expect("snapshot load")
        .expect("snapshot present");
    assert_eq!(
        snapshot.bilingual_mode,
        bookforge_core::BilingualMode::AppendText
    );
    assert_eq!(snapshot.bilingual_separator, " -- ");
    assert_eq!(
        snapshot.bilingual_style,
        bookforge_core::BilingualStyle::Prominent
    );
    assert_eq!(
        snapshot.bilingual_css.as_deref(),
        Some(".bookforge-translation { color: #123456; }\n")
    );
}

#[test]
fn cli_translate_append_block_keeps_glossary_terms_in_run_snapshot() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let input = fixture_input();
    let output = temp.path().join("out.epub");
    let events = temp.path().join("events.jsonl");
    let glossary = temp.path().join("glossary.toml");
    fs::write(
        &glossary,
        r#"[meta]
schema_version = 1
source_language = "English"
target_language = "Italian"

[meta.scope]
kind = "book"
id = "fellowship"

[[term]]
source = "Ivan Ilych"
target = "Ivan Ilic"
category = "person"
case_sensitive = true
"#,
    )
    .expect("glossary should write");

    bookforge()
        .current_dir(temp.path())
        .args([
            "translate",
            input.to_str().unwrap(),
            "--source",
            "English",
            "--target",
            "Italian",
            "--provider",
            "mock",
            "--model",
            "mock-prefix-target",
            "--profile",
            "v1-fast",
            "--book-id",
            "fellowship",
            "--glossary",
            glossary.to_str().unwrap(),
            "--mode",
            "append-block",
            "--ui",
            "quiet",
            "--progress-jsonl",
            events.to_str().unwrap(),
            "--out",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();

    let job_id = job_id_from_events(&events);
    let store =
        JobStore::open(temp.path().join(".bookforge/jobs.sqlite")).expect("store should open");
    let snapshot = store
        .load_job_config_snapshot(&job_id)
        .expect("snapshot load")
        .expect("snapshot present");
    assert_eq!(
        snapshot.bilingual_mode,
        bookforge_core::BilingualMode::AppendBlock
    );
    assert!(
        snapshot
            .glossary_terms
            .iter()
            .any(|term| term.source_text == "Ivan Ilych" && term.target_text == "Ivan Ilic"),
        "append-block runs should retain selected glossary terms in the same snapshot used for prompts"
    );
}

#[test]
fn cli_translate_with_style_sheet_persists_rendered_block_in_snapshot() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let input = fixture_input();
    let style_path = temp.path().join("style.toml");
    fs::write(
        &style_path,
        r#"[meta]
schema_version = 1
target_language = "Italian"

[meta.scope]
kind = "book"
id = "smoke"

[register]
narration = "literary"
dialogue_default = "tu"

[free_text]
instructions = "Maintain a literary register typical of Italian fiction translation."
"#,
    )
    .expect("style sheet should write");

    let output = temp.path().join("out.epub");
    let events = temp.path().join("events.jsonl");
    bookforge()
        .current_dir(temp.path())
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
            "--book-id",
            "smoke",
            "--style",
            style_path.to_str().unwrap(),
            "--ui",
            "quiet",
            "--progress-jsonl",
            events.to_str().unwrap(),
            "--out",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();

    let job_id = job_id_from_events(&events);
    let store =
        JobStore::open(temp.path().join(".bookforge/jobs.sqlite")).expect("store should open");
    let snapshot = store
        .load_job_config_snapshot(&job_id)
        .expect("snapshot load")
        .expect("snapshot present");
    assert!(
        !snapshot.style_rendered_block.is_empty(),
        "snapshot should capture a non-empty style block when --style is supplied"
    );
    assert!(
        snapshot.style_rendered_block.contains("Register: literary"),
        "rendered block must include configured register"
    );
    assert!(
        snapshot
            .style_rendered_block
            .contains("Dialogue default: tu"),
        "rendered block must include configured dialogue default"
    );
    assert!(
        snapshot
            .style_rendered_block
            .contains("Maintain a literary register"),
        "rendered block must include free-text instructions"
    );
    assert!(
        !snapshot.style_fingerprint.is_empty(),
        "snapshot should record a style fingerprint when style is active"
    );
}

#[test]
fn cli_translate_with_entities_persists_agreement_block_in_snapshot() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let input = fixture_input();
    let entities_path = temp.path().join("entities.toml");
    fs::write(
        &entities_path,
        r#"[meta]
schema_version = 1
source_language = "English"
target_language = "Italian"

[meta.scope]
kind = "book"
id = "fellowship"

[[entity]]
source_name = "Galadriel"
target_name = "Galadriel"
gender_target = "f"
role = "elf-queen"

[[entity]]
source_name = "the Ring"
target_name = "l'Anello"
gender_target = "m"
role = "object"
"#,
    )
    .expect("entities file should write");

    let output = temp.path().join("out.epub");
    let events = temp.path().join("events.jsonl");
    bookforge()
        .current_dir(temp.path())
        .args([
            "translate",
            input.to_str().unwrap(),
            "--source",
            "English",
            "--target",
            "Italian",
            "--provider",
            "mock",
            "--model",
            "mock-prefix-target",
            "--profile",
            "v1-fast",
            "--book-id",
            "fellowship",
            "--entities",
            entities_path.to_str().unwrap(),
            "--ui",
            "quiet",
            "--progress-jsonl",
            events.to_str().unwrap(),
            "--out",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();

    let job_id = job_id_from_events(&events);
    let store =
        JobStore::open(temp.path().join(".bookforge/jobs.sqlite")).expect("store should open");
    let snapshot = store
        .load_job_config_snapshot(&job_id)
        .expect("snapshot load")
        .expect("snapshot present");
    assert!(
        !snapshot.entities_rendered_block.is_empty(),
        "snapshot should capture a non-empty entity block when --entities is supplied"
    );
    assert!(
        snapshot
            .entities_rendered_block
            .contains("Galadriel: feminine"),
        "rendered block must list feminine entities"
    );
    assert!(
        snapshot
            .entities_rendered_block
            .contains("l'Anello (the Ring): masculine"),
        "rendered block must list masculine entities with source-name disambiguation"
    );
    assert!(
        !snapshot.entities_fingerprint.is_empty(),
        "snapshot should record an entity fingerprint when entities are active"
    );
}

#[test]
fn cli_translate_batch_mode_with_sliding_context_completes_without_deadlock() {
    // PR5: section-aware batching enables sliding context in batch mode.
    // Before this change, build_translation_batches could pack multiple
    // segments of the same chapter into one batch, and awaiting context
    // for a later segment would deadlock on a sibling in the same batch.
    // With section partitioning, no batch crosses a chapter, so the
    // sliding-context fence works correctly. This test just asserts the
    // happy path completes — the value is "doesn't hang", which the
    // 60-second test timeout enforces by failure.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let input = fixture_input();
    let output = temp.path().join("out.epub");
    let events = temp.path().join("events.jsonl");
    bookforge()
        .current_dir(temp.path())
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
            "v1-fast", // batch-enabled
            "--context-window",
            "3",
            "--context-scope",
            "chapter",
            "--ui",
            "quiet",
            "--progress-jsonl",
            events.to_str().unwrap(),
            "--out",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();
    assert!(output.exists(), "translated EPUB should exist");
}

#[test]
fn cli_translate_with_full_v1_3_stack_persists_all_three_blocks() {
    // Acceptance §6.9: a single translate run with --context-window,
    // --style, and --entities together must capture all three blocks in
    // the persisted snapshot. This is the gate that proves the three
    // PRs interoperate end-to-end (PR1 sliding context + PR2 style +
    // PR3 entities + PR4 plumbing).
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let input = fixture_input();
    let style_path = temp.path().join("style.toml");
    fs::write(
        &style_path,
        r#"[meta]
schema_version = 1
target_language = "Italian"

[meta.scope]
kind = "book"
id = "fellowship"

[register]
narration = "literary"
dialogue_default = "tu"

[free_text]
instructions = "Preserve em-dashes and ellipses."
"#,
    )
    .expect("style sheet should write");

    let entities_path = temp.path().join("entities.toml");
    fs::write(
        &entities_path,
        r#"[meta]
schema_version = 1
source_language = "English"
target_language = "Italian"

[meta.scope]
kind = "book"
id = "fellowship"

[[entity]]
source_name = "Galadriel"
target_name = "Galadriel"
gender_target = "f"

[[entity]]
source_name = "the Ring"
target_name = "l'Anello"
gender_target = "m"
"#,
    )
    .expect("entities file should write");

    let output = temp.path().join("out.epub");
    let events = temp.path().join("events.jsonl");
    bookforge()
        .current_dir(temp.path())
        .args([
            "translate",
            input.to_str().unwrap(),
            "--source",
            "English",
            "--target",
            "Italian",
            "--provider",
            "mock",
            "--model",
            "mock-prefix-target",
            "--profile",
            "v1-fast",
            "--book-id",
            "fellowship",
            "--context-window",
            "3",
            "--context-scope",
            "chapter",
            "--style",
            style_path.to_str().unwrap(),
            "--entities",
            entities_path.to_str().unwrap(),
            "--ui",
            "quiet",
            "--progress-jsonl",
            events.to_str().unwrap(),
            "--out",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();

    let job_id = job_id_from_events(&events);
    let store =
        JobStore::open(temp.path().join(".bookforge/jobs.sqlite")).expect("store should open");
    let snapshot = store
        .load_job_config_snapshot(&job_id)
        .expect("snapshot load")
        .expect("snapshot present");

    // Sliding context: settings round-trip.
    assert_eq!(snapshot.context_window, 3);
    assert_eq!(
        snapshot.context_scope,
        bookforge_core::config::ContextScope::Chapter
    );

    // Style sheet: block + fingerprint present.
    assert!(!snapshot.style_rendered_block.is_empty());
    assert!(snapshot.style_rendered_block.contains("Register: literary"));
    assert!(!snapshot.style_fingerprint.is_empty());

    // Entity sheet: block + fingerprint present.
    assert!(!snapshot.entities_rendered_block.is_empty());
    assert!(
        snapshot
            .entities_rendered_block
            .contains("l'Anello (the Ring): masculine")
    );
    assert!(!snapshot.entities_fingerprint.is_empty());

    // Style and entity fingerprints are independent — domain-separator test.
    assert_ne!(snapshot.style_fingerprint, snapshot.entities_fingerprint);
}

#[test]
fn cli_entities_import_then_show_matches_input() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let entities_path = temp.path().join("entities.toml");
    fs::write(
        &entities_path,
        r#"[meta]
schema_version = 1
source_language = "English"
target_language = "Italian"

[meta.scope]
kind = "global"

[[entity]]
source_name = "Gandalf"
target_name = "Gandalf"
gender_target = "m"
"#,
    )
    .expect("entities file should write");

    bookforge()
        .current_dir(temp.path())
        .args(["entities", "import", entities_path.to_str().unwrap()])
        .assert()
        .success();

    let assert = bookforge()
        .current_dir(temp.path())
        .args([
            "entities",
            "show",
            "--source-language",
            "English",
            "--target-language",
            "Italian",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    assert!(
        stdout.contains("Gandalf: masculine"),
        "entities show should render the imported entry; got: {stdout}"
    );
}

#[test]
fn cli_style_import_then_show_matches_input() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let style_path = temp.path().join("style.toml");
    fs::write(
        &style_path,
        r#"[meta]
schema_version = 1
target_language = "Italian"

[meta.scope]
kind = "global"

[register]
narration = "neutral"
"#,
    )
    .expect("style sheet should write");

    bookforge()
        .current_dir(temp.path())
        .args(["style", "import", style_path.to_str().unwrap()])
        .assert()
        .success();

    let assert = bookforge()
        .current_dir(temp.path())
        .args(["style", "show", "--language", "Italian"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    assert!(
        stdout.contains("Register: neutral"),
        "show output should render the imported register; got: {stdout}"
    );
}

#[test]
fn cli_translate_mock_with_same_glossary_is_bit_identical() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let input = fixture_input();
    let glossary = temp.path().join("glossary.toml");
    fs::write(
        &glossary,
        r#"[meta]
schema_version = 1
source_language = "English"
target_language = "Italian"

[meta.scope]
kind = "book"
id = "smoke"

[[term]]
source = "Aragorn"
target = "Aragorn"
category = "person"
case_sensitive = true
"#,
    )
    .expect("glossary should write");

    let first = temp.path().join("a.epub");
    let second = temp.path().join("b.epub");
    for output in [&first, &second] {
        bookforge()
            .current_dir(temp.path())
            .args([
                "translate",
                input.to_str().unwrap(),
                "--source",
                "English",
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
                "--book-id",
                "smoke",
                "--glossary",
                glossary.to_str().unwrap(),
                "--out",
                output.to_str().unwrap(),
            ])
            .assert()
            .success();
    }

    assert_eq!(
        fs::read(&first).expect("first EPUB should read"),
        fs::read(&second).expect("second EPUB should read"),
        "same input and glossary should produce bit-identical EPUBs"
    );
}

#[test]
fn cli_glossary_import_export_reimports_identical_terms() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let imported = temp.path().join("imported.toml");
    let exported = temp.path().join("exported.toml");
    fs::write(
        &imported,
        r#"[meta]
schema_version = 1
source_language = "English"
target_language = "Italian"

[meta.scope]
kind = "book"
id = "roundtrip"

[[term]]
source = "Aragorn"
target = "Aragorn"
category = "person"
case_sensitive = true
status = "user_seeded"
source_count = 4

[[term]]
source = "Shire"
target = "Contea"
category = "place"
notes = "Canonical place name."
status = "accepted"
source_count = 2
"#,
    )
    .expect("glossary should write");

    bookforge()
        .current_dir(temp.path())
        .args(["glossary", "import", imported.to_str().unwrap()])
        .assert()
        .success();

    let store_path = temp.path().join(".bookforge/jobs.sqlite");
    let store = JobStore::open(&store_path).expect("store opens");
    let original = store
        .list_glossary_terms(roundtrip_filter())
        .expect("terms should list");
    assert_eq!(original.len(), 2);
    assert!(
        original
            .iter()
            .any(|term| term.status == bookforge_core::GlossaryStatus::UserSeeded)
    );
    assert!(
        original
            .iter()
            .any(|term| term.status == bookforge_core::GlossaryStatus::Accepted)
    );
    drop(store);

    bookforge()
        .current_dir(temp.path())
        .args([
            "glossary",
            "export",
            exported.to_str().unwrap(),
            "--scope",
            "book",
            "--scope-id",
            "roundtrip",
            "--language",
            "English->Italian",
        ])
        .assert()
        .success();

    bookforge()
        .current_dir(temp.path())
        .args([
            "glossary",
            "clear",
            "--scope",
            "book",
            "--scope-id",
            "roundtrip",
        ])
        .assert()
        .success();
    bookforge()
        .current_dir(temp.path())
        .args(["glossary", "import", exported.to_str().unwrap()])
        .assert()
        .success();

    let store = JobStore::open(&store_path).expect("store opens");
    let roundtripped = store
        .list_glossary_terms(roundtrip_filter())
        .expect("terms should list");
    assert_eq!(
        normalized_terms(original),
        normalized_terms(roundtripped),
        "exported TOML should reimport to the same glossary term fields"
    );
}

#[test]
fn cli_glossary_extract_candidates_stores_auto_candidates() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let input = fixture_input();

    bookforge()
        .current_dir(temp.path())
        .args([
            "glossary",
            "extract-candidates",
            input.to_str().unwrap(),
            "--book-id",
            "ivan",
            "--source-lang",
            "English",
            "--target-lang",
            "Italian",
            "--min-count",
            "4",
        ])
        .assert()
        .success();

    let store = JobStore::open(temp.path().join(".bookforge/jobs.sqlite")).expect("store opens");
    let candidates = store
        .list_glossary_candidates("ivan", "English", "Italian")
        .expect("candidates should list");
    assert!(
        !candidates.is_empty(),
        "extract-candidates should persist at least one auto-candidate from the fixture EPUB"
    );
    assert!(
        candidates
            .iter()
            .all(|candidate| candidate.source_count >= 4),
        "all stored candidates should satisfy the requested minimum source count"
    );
    assert!(candidates.iter().all(|candidate| {
        candidate.target_text.is_none() && candidate.status == GlossaryStatus::AutoCandidate
    }));
}

#[test]
fn cli_glossary_review_candidates_accepts_sets_and_rejects() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let input = fixture_input();
    let store_path = temp.path().join(".bookforge/jobs.sqlite");
    let store = JobStore::open(&store_path).expect("store opens");
    store
        .upsert_glossary_candidates(
            "manual-review",
            "English",
            "Italian",
            &[
                NewGlossaryCandidate {
                    source_text: "Aragorn",
                    category: GlossaryCategory::Other,
                    source_count: 10,
                },
                NewGlossaryCandidate {
                    source_text: "Mount Doom",
                    category: GlossaryCategory::Other,
                    source_count: 9,
                },
                NewGlossaryCandidate {
                    source_text: "Ivan Ilych",
                    category: GlossaryCategory::Other,
                    source_count: 8,
                },
            ],
        )
        .expect("candidates should insert");
    drop(store);

    bookforge()
        .current_dir(temp.path())
        .args([
            "glossary",
            "review-candidates",
            "manual-review",
            "--language",
            "English->Italian",
        ])
        .write_stdin("accept 1\nset 1 \"Monte Fato\"\nreject 1\nquit\n")
        .assert()
        .success();

    let store = JobStore::open(&store_path).expect("store opens");
    let terms = store
        .list_glossary_terms(GlossaryFilter {
            scope_kind: Some(bookforge_core::GlossaryScopeKind::Book),
            scope_id: Some("manual-review"),
            source_language: Some("English"),
            target_language: Some("Italian"),
            active_only: false,
        })
        .expect("terms should list");

    assert!(terms.iter().any(|term| {
        term.source_text == "Aragorn"
            && term.target_text == "Aragorn"
            && term.status == GlossaryStatus::Accepted
    }));
    assert!(terms.iter().any(|term| {
        term.source_text == "Mount Doom"
            && term.target_text == "Monte Fato"
            && term.status == GlossaryStatus::Accepted
    }));
    assert!(terms.iter().any(|term| {
        term.source_text == "Ivan Ilych" && term.status == GlossaryStatus::Rejected
    }));

    bookforge()
        .current_dir(temp.path())
        .args([
            "glossary",
            "extract-candidates",
            input.to_str().unwrap(),
            "--book-id",
            "manual-review",
            "--source-lang",
            "English",
            "--target-lang",
            "Italian",
            "--min-count",
            "1",
        ])
        .assert()
        .success();
    let terms_after_rerun = store
        .list_glossary_terms(GlossaryFilter {
            scope_kind: Some(bookforge_core::GlossaryScopeKind::Book),
            scope_id: Some("manual-review"),
            source_language: Some("English"),
            target_language: Some("Italian"),
            active_only: false,
        })
        .expect("terms should list after rerun");
    assert_eq!(
        terms_after_rerun
            .iter()
            .filter(|term| term.source_text == "Ivan Ilych")
            .count(),
        1,
        "rejected candidates should not be resurrected"
    );
}

#[test]
fn cli_glossary_review_candidates_requires_language_when_ambiguous() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let store_path = temp.path().join(".bookforge/jobs.sqlite");
    let store = JobStore::open(&store_path).expect("store opens");
    for (source, target) in [("English", "Italian"), ("English", "French")] {
        store
            .upsert_glossary_candidates(
                "ambiguous-review",
                source,
                target,
                &[NewGlossaryCandidate {
                    source_text: "Ivan Ilych",
                    category: GlossaryCategory::Other,
                    source_count: 8,
                }],
            )
            .expect("candidate should insert");
    }
    drop(store);

    let assert = bookforge()
        .current_dir(temp.path())
        .args(["glossary", "review-candidates", "ambiguous-review"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(stderr.contains("multiple candidate language pairs exist"));
    assert!(stderr.contains("English->French"));
    assert!(stderr.contains("English->Italian"));
}

#[test]
fn cli_translate_json_mode_emits_valid_jsonl_stdout_and_file_log() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let input = fixture_input();
    let output = temp.path().join("json.epub");
    let events = temp.path().join("json-events.jsonl");
    let assert = bookforge()
        .current_dir(temp.path())
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
            "json",
            "--progress-jsonl",
            events.to_str().unwrap(),
            "--out",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    let stdout_events = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()
        .expect("stdout should be valid JSONL");
    assert!(
        stdout_events
            .iter()
            .any(|event| event.get("JobCreated").is_some()),
        "stdout JSONL should include job creation"
    );

    let file_events = read_jsonl(&events);
    assert!(
        file_events
            .iter()
            .any(|event| event.get("TranslationFinished").is_some()),
        "file JSONL should include completion"
    );
}

fn roundtrip_filter<'a>() -> GlossaryFilter<'a> {
    GlossaryFilter {
        scope_kind: Some(bookforge_core::GlossaryScopeKind::Book),
        scope_id: Some("roundtrip"),
        source_language: Some("English"),
        target_language: Some("Italian"),
        active_only: false,
    }
}

fn normalized_terms(mut terms: Vec<GlossaryTerm>) -> Vec<GlossaryTerm> {
    for term in &mut terms {
        term.id = None;
    }
    terms.sort_by(|a, b| {
        (
            a.scope_kind.as_str(),
            a.scope_id.as_deref(),
            a.source_text.as_str(),
            a.source_language.as_str(),
            a.target_language.as_str(),
        )
            .cmp(&(
                b.scope_kind.as_str(),
                b.scope_id.as_deref(),
                b.source_text.as_str(),
                b.source_language.as_str(),
                b.target_language.as_str(),
            ))
    });
    terms
}

#[test]
fn cli_status_after_translate_reports_succeeded_job() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let run = translate_quiet(&temp, "mock-prefix-target");

    let assert = bookforge()
        .current_dir(temp.path())
        .args(["status", &run.job_id])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);

    assert!(stdout.contains(&format!("Job: {}", run.job_id)));
    assert!(stdout.contains("Status: succeeded"));
    assert!(stdout.contains("Segments:"));
    assert!(stdout.contains("Output:"));
    assert!(stdout.contains("Event log:"));
    assert!(stdout.contains("Report:"));
    assert!(stdout.contains("Performance:"));
}

#[test]
fn cli_correct_persists_manual_blocks_and_rebuilds_output() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let run = translate_quiet(&temp, "mock-prefix-target");
    let store = JobStore::open(temp.path().join(".bookforge/jobs.sqlite")).expect("store opens");
    let segment = store
        .load_terminal_segment_translations(&run.job_id)
        .expect("translations should load")
        .into_iter()
        .next()
        .expect("fixture should produce a translated segment");
    let corrected_blocks = segment
        .blocks
        .iter()
        .map(|block| {
            serde_json::json!({
                "block_id": block.block_id.0,
                "text": format!("MANUAL {}", block.text),
            })
        })
        .collect::<Vec<_>>();
    let correction_path = temp.path().join("correction.json");
    fs::write(
        &correction_path,
        serde_json::to_vec_pretty(&serde_json::json!({ "blocks": corrected_blocks }))
            .expect("correction should serialize"),
    )
    .expect("correction file should write");

    let report_json_path = run.report.with_extension("json");
    let report_before: serde_json::Value = serde_json::from_slice(
        &fs::read(&report_json_path).expect("QA report should exist after translate"),
    )
    .expect("QA report should parse before correction");
    assert_eq!(
        report_before["corrected_segments"], 0,
        "no segment should be marked corrected before the `correct` run"
    );

    bookforge()
        .current_dir(temp.path())
        .args([
            "correct",
            &run.job_id,
            "--segment",
            &segment.segment_id,
            "--from-file",
            correction_path.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("Job status: succeeded"));

    let corrected = store
        .load_terminal_segment_translations(&run.job_id)
        .expect("corrected translations should load")
        .into_iter()
        .find(|translation| translation.segment_id == segment.segment_id)
        .expect("corrected segment should remain present");
    assert!(corrected.human_corrected);
    assert_eq!(corrected.provider, "manual");
    assert!(
        corrected
            .blocks
            .iter()
            .all(|block| block.text.starts_with("MANUAL "))
    );
    assert!(
        run.output.exists(),
        "correct should rebuild the translated EPUB"
    );

    // The QA report artifact (report.rs's `write_report`, auto-written at
    // translate/resume finalization) is regenerated in place by
    // `correct_job_segment` so it does not go stale after a manual
    // correction — see `regenerate_report_after_correction` in
    // `commands/translate/reporting.rs`.
    let report_after: serde_json::Value = serde_json::from_slice(
        &fs::read(&report_json_path).expect("QA report should still exist after correction"),
    )
    .expect("QA report should parse after correction");
    assert_eq!(
        report_after["corrected_segments"], 1,
        "QA report should be refreshed to reflect the manual correction"
    );
    let report_markdown_after = fs::read_to_string(&run.report)
        .expect("QA report markdown should still exist after correction");
    assert!(
        report_markdown_after.contains("Manually corrected: 1"),
        "QA report markdown should be refreshed with the corrected-segment count: {report_markdown_after}"
    );

    let review_dir = temp.path().join("corrected-review");
    bookforge()
        .current_dir(temp.path())
        .args(["review", &run.job_id, "--out", review_dir.to_str().unwrap()])
        .assert()
        .success();
    let review: serde_json::Value = serde_json::from_slice(
        &fs::read(review_dir.join("review.json")).expect("review JSON should exist"),
    )
    .expect("review should parse");
    let reviewed = review["segments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["segment_id"] == segment.segment_id)
        .expect("corrected segment should be in review");
    assert_eq!(reviewed["human_corrected"], true);
    assert!(reviewed["corrected_at"].is_string());

    assert_no_staged_output_files(&run.output);
}

/// Lists the output directory and fails if any staged rebuild artifact (the
/// `<stem>.staged-<pid>-<nonce><ext>` sibling that `correct_job_segment`
/// rebuilds into before atomically swapping it over the real output) was left
/// behind. A leftover staged file would mean the atomic swap either never ran
/// or failed to clean up after itself.
fn assert_no_staged_output_files(output: &Path) {
    let stem = output
        .file_stem()
        .and_then(|value| value.to_str())
        .expect("output should have a file stem");
    let dir = output
        .parent()
        .expect("output should have a parent directory");
    let stray = fs::read_dir(dir)
        .expect("output directory should be readable")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(&format!("{stem}.staged-")))
        .collect::<Vec<_>>();
    assert!(
        stray.is_empty(),
        "no staged correction rebuild artifacts should remain in {}, found: {:?}",
        dir.display(),
        stray
    );
}

#[test]
fn cli_correct_with_marker_violation_leaves_db_and_output_unchanged() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let run = translate_quiet(&temp, "mock-prefix-target");
    let store = JobStore::open(temp.path().join(".bookforge/jobs.sqlite")).expect("store opens");
    let segment = store
        .load_terminal_segment_translations(&run.job_id)
        .expect("translations should load")
        .into_iter()
        .next()
        .expect("fixture should produce a translated segment");
    let original_output_bytes = fs::read(&run.output).expect("output should exist before correct");

    // Blanking a block's translation while its source text is non-empty trips
    // the "empty_translation" structural-validation error, which is a
    // deterministic way to exercise the failure path without mocking the
    // filesystem.
    let corrected_blocks = segment
        .blocks
        .iter()
        .enumerate()
        .map(|(index, block)| {
            let text = if index == 0 {
                String::new()
            } else {
                block.text.clone()
            };
            serde_json::json!({
                "block_id": block.block_id.0,
                "text": text,
            })
        })
        .collect::<Vec<_>>();
    let correction_path = temp.path().join("bad-correction.json");
    fs::write(
        &correction_path,
        serde_json::to_vec_pretty(&serde_json::json!({ "blocks": corrected_blocks }))
            .expect("correction should serialize"),
    )
    .expect("correction file should write");

    let assert = bookforge()
        .current_dir(temp.path())
        .args([
            "correct",
            &run.job_id,
            "--segment",
            &segment.segment_id,
            "--from-file",
            correction_path.to_str().unwrap(),
        ])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains("manual correction violates EPUB marker constraints"),
        "unexpected stderr: {stderr}"
    );

    let unchanged = store
        .load_terminal_segment_translations(&run.job_id)
        .expect("translations should still load")
        .into_iter()
        .find(|translation| translation.segment_id == segment.segment_id)
        .expect("segment should remain present");
    assert!(
        !unchanged.human_corrected,
        "failed correction must not be recorded as human-corrected"
    );
    assert_eq!(
        unchanged.provider, segment.provider,
        "failed correction must not overwrite the original provider"
    );
    assert_eq!(
        unchanged.blocks, segment.blocks,
        "failed correction must not change stored block translations"
    );

    let output_bytes_after = fs::read(&run.output).expect("output should still exist");
    assert_eq!(
        output_bytes_after, original_output_bytes,
        "failed correction must leave the existing output EPUB byte-identical"
    );

    assert_no_staged_output_files(&run.output);
}

#[test]
fn cli_tail_after_translate_prints_recent_events() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let run = translate_quiet(&temp, "mock-prefix-target");

    let assert = bookforge()
        .current_dir(temp.path())
        .args(["tail", &run.job_id, "--last", "3"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);

    assert!(stdout.contains("Last "));
    assert!(stdout.contains("Reconstructed state:"));
}

#[test]
fn cli_tail_json_outputs_valid_jsonl() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let run = translate_quiet(&temp, "mock-prefix-target");

    let assert = bookforge()
        .current_dir(temp.path())
        .args(["tail", &run.job_id, "--last", "5", "--json"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);

    assert!(!stdout.contains("Last "));
    let events = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()
        .expect("tail --json should emit valid JSONL");
    assert!(!events.is_empty(), "tail --json should emit recent events");
}

#[test]
fn cli_resume_missing_job_fails_clearly() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let assert = bookforge()
        .current_dir(temp.path())
        .args(["resume", "job_missing"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);

    assert!(stderr.contains("job 'job_missing' was not found"));
}

#[test]
fn cli_resume_reuses_checkpointed_segments() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let run = translate_quiet(&temp, "mock-prefix-target");
    let resume_events = temp.path().join("resume-events.jsonl");
    let store = JobStore::open(temp.path().join(".bookforge/jobs.sqlite")).expect("store opens");
    let segment_ids = store
        .segment_records(&run.job_id)
        .expect("segments should load")
        .into_iter()
        .map(|record| record.id)
        .collect::<Vec<_>>();
    assert!(!segment_ids.is_empty(), "fixture should produce segments");
    let retry_id = segment_ids[0].clone();
    store
        .mark_segment_failed(&run.job_id, &retry_id, "force resume")
        .expect("segment should be marked failed");

    let resume = bookforge()
        .current_dir(temp.path())
        .args([
            "resume",
            &run.job_id,
            "--ui",
            "quiet",
            "--progress-jsonl",
            resume_events.to_str().unwrap(),
        ])
        .assert()
        .success();
    assert!(
        resume.get_output().stdout.is_empty(),
        "resume --ui quiet should not write human stdout"
    );

    let events = read_jsonl(&resume_events);
    let segment_finished = events
        .iter()
        .filter_map(|event| event.get("SegmentFinished"))
        .collect::<Vec<_>>();
    assert_eq!(
        segment_finished.len(),
        1,
        "resume should translate only the failed segment"
    );
    assert_eq!(
        segment_finished[0]
            .get("segment_id")
            .and_then(|value| value.as_str()),
        Some(retry_id.as_str())
    );
}

#[test]
fn cli_resume_force_relaunches_dead_paused_job() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let run = translate_quiet(&temp, "mock-prefix-target");
    let resume_events = temp.path().join("force-resume-events.jsonl");
    let store = JobStore::open(temp.path().join(".bookforge/jobs.sqlite")).expect("store opens");
    let retry_id = store
        .segment_records(&run.job_id)
        .expect("segments should load")
        .into_iter()
        .next()
        .expect("fixture should produce segments")
        .id;
    store
        .mark_segment_failed(&run.job_id, &retry_id, "force dead paused resume")
        .expect("segment should be marked failed");
    store
        .mark_job_paused(&run.job_id)
        .expect("job should be marked paused");
    let control_path = temp
        .path()
        .join(".bookforge/runs")
        .join(&run.job_id)
        .join("control");
    write_control_file(&control_path, ControlCommand::Pause).expect("pause control should write");
    drop(store);

    bookforge()
        .current_dir(temp.path())
        .args([
            "resume",
            &run.job_id,
            "--force",
            "--ui",
            "quiet",
            "--progress-jsonl",
            resume_events.to_str().unwrap(),
        ])
        .assert()
        .success();

    assert_eq!(
        read_control_file(&control_path).expect("control file should read"),
        ControlCommand::Run,
        "forced resume should clear stale pause control"
    );
    let events = read_jsonl(&resume_events);
    let segment_finished = segment_finished_ids(&events);
    assert_eq!(
        segment_finished,
        vec![retry_id],
        "forced resume should translate only the failed segment"
    );
    let store = JobStore::open(temp.path().join(".bookforge/jobs.sqlite")).expect("store opens");
    assert_ne!(
        store
            .get_job(&run.job_id)
            .expect("job should load")
            .expect("job should exist")
            .status,
        "paused",
        "forced resume should leave the paused state"
    );
}

#[test]
fn cli_pause_and_resume_live_mock_run() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let events = temp.path().join("events.jsonl");
    let output = temp.path().join("out.epub");
    // The mock parks every request until `release` exists, so the pause below
    // lands at an observed point — one request started, none able to finish —
    // instead of racing the mock's fixed delay against machine load.
    let release = temp.path().join("mock-release");
    let mut child = spawn_gated_mock_translate(&temp, &events, &output, &release);
    let job_id = wait_for_job_id_in_events(&events, &mut child);
    wait_for_first_batch_request(&events, &mut child);
    let control_path = temp
        .path()
        .join(".bookforge/runs")
        .join(&job_id)
        .join("control");
    write_control_file(&control_path, ControlCommand::Pause).expect("pause control should write");
    assert_eq!(
        fs::read_to_string(&control_path).expect("pause control file should exist"),
        "pause\n"
    );
    wait_for_job_status(&temp, &job_id, "paused");
    let paused_events = wait_for_event_count(&events, "JobPaused", 1);
    assert_eq!(
        batch_request_started_count(&paused_events),
        1,
        "batch pause should not start another provider request while one is in flight"
    );
    // Guards the setup itself: if the gate ever stops holding the request, this
    // fails loudly instead of quietly turning the assertions above back into a
    // race against the mock delay.
    assert_eq!(
        batch_request_finished_count(&paused_events),
        0,
        "gated mock should still hold the first request in flight when the pause lands"
    );

    // The pause is acknowledged, so release the gated request. A request already
    // in flight when the pause landed may still record its segment after
    // JobPaused is emitted — an in-flight request cannot be un-sent — but once
    // it settles the dispatch loop must park rather than start a new one.
    // `RequestFinished` is emitted after the worker is joined and the control
    // boundary re-polled, which makes it the point where a second request would
    // have been dispatched had the pause not taken effect.
    fs::write(&release, "release").expect("mock release file should write");
    let settled = wait_for_events(&events, |events| batch_request_finished_count(events) >= 1);
    assert_eq!(
        batch_request_started_count(&settled),
        1,
        "paused batch run should not start another provider request while parked"
    );
    assert_no_duplicate_segments(&segment_finished_ids(&settled));
    wait_for_job_status(&temp, &job_id, "paused");

    bookforge()
        .current_dir(temp.path())
        .args(["resume", &job_id, "--ui", "quiet"])
        .assert()
        .success();

    let status = child.wait().expect("translate child should exit");
    assert!(status.success(), "translate child failed: {status}");
    let final_events = wait_for_event_count(&events, "TranslationFinished", 1);
    assert!(
        event_count(&final_events, "JobResumed") >= 1,
        "resume event should be logged"
    );
    assert_no_duplicate_segments(&segment_finished_ids(&final_events));
    assert!(output.exists(), "resumed live run should write output");
}

/// A parked run must still honour a stop.
///
/// Pause deliberately has no timeout — a paused job waits indefinitely, which is
/// the feature — so `stop` is the only in-band way to terminate one. If that
/// transition ever regresses, a paused job becomes unkillable except by
/// `TerminateProcess`, and every abandoned pause turns into a permanent orphan.
#[test]
fn cli_stop_terminates_a_parked_paused_run() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let events = temp.path().join("paused-stop-events.jsonl");
    let output = temp.path().join("paused-stop.epub");
    let release = temp.path().join("mock-release");
    let mut child = spawn_gated_mock_translate(&temp, &events, &output, &release);
    let job_id = wait_for_job_id_in_events(&events, &mut child);
    wait_for_first_batch_request(&events, &mut child);
    let control_path = control_path(&temp, &job_id);
    write_control_file(&control_path, ControlCommand::Pause).expect("pause control should write");
    wait_for_job_status(&temp, &job_id, "paused");
    wait_for_event_count(&events, "JobPaused", 1);

    // Let the gated request settle so the run is parked in the paused dispatch
    // loop with no work in flight — the state an abandoned pause leaves behind.
    fs::write(&release, "release").expect("mock release file should write");
    wait_for_events(&events, |events| batch_request_finished_count(events) >= 1);

    write_control_file(&control_path, ControlCommand::Stop).expect("stop control should write");
    let status = wait_for_child_exit(&mut child, Duration::from_secs(30))
        .expect("paused run should exit after a stop control; it stayed parked instead");
    assert!(status.success(), "stopped translate child failed: {status}");
    wait_for_job_status(&temp, &job_id, "stopped");
}

#[test]
fn cli_live_reconfigure_updates_later_single_requests_and_cleans_runtime_files() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let events = temp.path().join("single-live-reconfigure-events.jsonl");
    let output = temp.path().join("single-live-reconfigure.epub");
    let release = temp.path().join("single-live-reconfigure-release");
    let mut child = spawn_gated_single_mock_translate(&temp, &events, &output, &release);
    let job_id = wait_for_job_id_in_events(&events, &mut child);
    wait_for_first_request(&events, &mut child);

    bookforge()
        .current_dir(temp.path())
        .args([
            "reconfigure",
            &job_id,
            "--concurrency",
            "2",
            "--provider-max-attempts",
            "3",
            "--batch-max-output-tokens",
            "1024",
        ])
        .assert()
        .success();
    // A successful reconfigure command means the sidecar is durable, not that
    // the live watch channel has published it. Keep request 1 parked until the
    // worker acknowledges revision 1 so later dispatch cannot race the watcher.
    wait_for_child_events(&events, &mut child, |events| {
        event_count(events, "RuntimeConfigChanged") >= 1
    });

    assert!(
        request_finished_ids(&read_jsonl(&events)).is_empty(),
        "the release gate should keep the baseline request in flight during reconfigure"
    );
    fs::write(&release, "release").expect("mock release file should write");
    let status = child.wait().expect("translate child should exit");
    assert!(status.success(), "translate child failed: {status}");
    let final_events = wait_for_event_count(&events, "TranslationFinished", 1);
    let requests = request_started_payloads(&final_events);
    assert!(requests.len() > 1, "fixture should make several requests");
    assert_eq!(
        requests[0]
            .get("runtime_config_revision")
            .and_then(serde_json::Value::as_u64),
        Some(0),
        "the in-flight request must retain the baseline revision"
    );
    assert!(requests.iter().skip(1).any(|request| {
        request
            .get("runtime_config_revision")
            .and_then(serde_json::Value::as_u64)
            == Some(1)
            && request
                .get("provider_max_attempts")
                .and_then(serde_json::Value::as_u64)
                == Some(3)
    }));
    assert_no_duplicate_segments(&segment_finished_ids(&final_events));
    assert!(output.exists());
    assert!(
        !overrides_path(&temp, &job_id).exists(),
        "successful completion should consume the sidecar"
    );
    assert!(
        !runtime_path(&temp, &job_id).exists(),
        "clean worker exit should remove its lease"
    );
}

#[test]
fn cli_live_reconfigure_repartitions_pending_batch_work() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let events = temp.path().join("batch-live-reconfigure-events.jsonl");
    let output = temp.path().join("batch-live-reconfigure.epub");
    let release = temp.path().join("batch-live-reconfigure-release");
    let mut child = spawn_gated_mock_translate(&temp, &events, &output, &release);
    let job_id = wait_for_job_id_in_events(&events, &mut child);
    wait_for_first_batch_request(&events, &mut child);

    bookforge()
        .current_dir(temp.path())
        .args([
            "reconfigure",
            &job_id,
            "--batch-max-items",
            "4",
            "--batch-target-tokens",
            "100000",
            "--concurrency",
            "2",
            "--provider-max-attempts",
            "4",
            "--batch-max-output-tokens",
            "1024",
            "--adaptive-concurrency",
            "false",
            "--adaptive-batch-sizing",
            "false",
        ])
        .assert()
        .success();
    // The release gate protects only the in-flight batch. Wait for the worker's
    // acknowledgement as well, otherwise pending work can repartition under
    // revision 0 when the watcher loses a scheduling race.
    wait_for_child_events(&events, &mut child, |events| {
        event_count(events, "RuntimeConfigChanged") >= 1
    });

    assert_eq!(
        batch_request_finished_count(&read_jsonl(&events)),
        0,
        "the release gate should keep the baseline batch in flight during reconfigure"
    );
    fs::write(&release, "release").expect("mock release file should write");
    let status = child.wait().expect("translate child should exit");
    assert!(status.success(), "translate child failed: {status}");
    let final_events = wait_for_event_count(&events, "TranslationFinished", 1);
    let requests = request_started_payloads(&final_events);
    assert!(requests.len() > 1);
    assert_eq!(
        requests[0]
            .get("runtime_config_revision")
            .and_then(serde_json::Value::as_u64),
        Some(0)
    );
    assert!(requests.iter().skip(1).any(|request| {
        request
            .get("runtime_config_revision")
            .and_then(serde_json::Value::as_u64)
            == Some(1)
            && request
                .get("provider_max_attempts")
                .and_then(serde_json::Value::as_u64)
                == Some(4)
            && request
                .get("items")
                .and_then(serde_json::Value::as_u64)
                .is_some_and(|items| items > 1)
    }));
    assert_no_duplicate_segments(&segment_finished_ids(&final_events));
    assert!(!overrides_path(&temp, &job_id).exists());
    assert!(!runtime_path(&temp, &job_id).exists());
}

#[test]
fn cli_stop_preserves_runtime_overrides_and_resume_consumes_them() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let events = temp.path().join("stop-runtime-reconfigure-events.jsonl");
    let output = temp.path().join("stop-runtime-reconfigure.epub");
    let release = temp.path().join("stop-runtime-reconfigure-release");
    let mut child = spawn_gated_mock_translate(&temp, &events, &output, &release);
    let job_id = wait_for_job_id_in_events(&events, &mut child);
    wait_for_first_batch_request(&events, &mut child);

    bookforge()
        .current_dir(temp.path())
        .args([
            "reconfigure",
            &job_id,
            "--concurrency",
            "2",
            "--provider-max-attempts",
            "4",
            "--batch-max-items",
            "3",
        ])
        .assert()
        .success();
    wait_for_child_events(&events, &mut child, |events| {
        event_count(events, "RuntimeConfigChanged") >= 1
    });
    write_control_file(&control_path(&temp, &job_id), ControlCommand::Stop)
        .expect("stop control should write");
    fs::write(&release, "release").expect("mock release file should write");

    let status = child.wait().expect("translate child should exit");
    assert!(status.success(), "translate child failed: {status}");
    assert_eq!(job_status(&temp, &job_id), "stopped");
    let stopped_events = read_jsonl(&events);
    assert_eq!(event_count(&stopped_events, "TranslationFinished"), 0);
    let initially_finished = segment_finished_ids(&stopped_events);
    assert!(
        overrides_path(&temp, &job_id).exists(),
        "Stop must preserve durable runtime overrides for resume"
    );
    assert!(
        !runtime_path(&temp, &job_id).exists(),
        "the stopped worker exited cleanly and should release its lease"
    );

    let resume_events = temp.path().join("stop-runtime-resume-events.jsonl");
    bookforge()
        .current_dir(temp.path())
        .args([
            "resume",
            &job_id,
            "--ui",
            "quiet",
            "--progress-jsonl",
            resume_events.to_str().unwrap(),
        ])
        .assert()
        .success();

    let resumed = wait_for_event_count(&resume_events, "TranslationFinished", 1);
    assert!(request_started_payloads(&resumed).iter().any(|request| {
        request
            .get("runtime_config_revision")
            .and_then(serde_json::Value::as_u64)
            == Some(1)
            && request
                .get("provider_max_attempts")
                .and_then(serde_json::Value::as_u64)
                == Some(4)
    }));
    let resumed_finished = segment_finished_ids(&resumed);
    for id in initially_finished {
        assert!(
            !resumed_finished.contains(&id),
            "resume retranslated already checkpointed segment {id}"
        );
    }
    assert!(
        !overrides_path(&temp, &job_id).exists(),
        "successful resume should consume the sidecar"
    );
    assert!(!runtime_path(&temp, &job_id).exists());
}

#[test]
fn killed_worker_leaves_a_stale_recoverable_runtime_lease() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let events = temp.path().join("crash-runtime-lease-events.jsonl");
    let output = temp.path().join("crash-runtime-lease.epub");
    let mut child = spawn_controlled_mock_translate(&temp, &events, &output);
    let job_id = wait_for_job_id_in_events(&events, &mut child);
    let lease_path = runtime_path(&temp, &job_id);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !lease_path.exists() {
        assert!(Instant::now() < deadline, "runtime lease should appear");
        thread::sleep(Duration::from_millis(20));
    }

    child.kill().expect("worker should be killable");
    let _ = child.wait().expect("killed worker should reap");
    assert!(
        lease_path.exists(),
        "a process crash cannot clean its lease; the stale file enables recovery"
    );
    thread::sleep(Duration::from_millis(3_150));
    let lease: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&lease_path).expect("stale lease should remain readable"),
    )
    .expect("runtime lease should remain valid JSON");
    let heartbeat = lease["heartbeat_at_ms"]
        .as_u64()
        .expect("lease heartbeat should be numeric");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_millis() as u64;
    assert!(
        now.saturating_sub(heartbeat) > 3_000,
        "killed worker lease should age beyond the dashboard stale threshold"
    );
}

#[test]
fn cli_finalize_stages_snapshot_runtime_settings_at_stage_boundaries() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let events = temp.path().join("stage-live-reconfigure-events.jsonl");
    let output = temp.path().join("stage-live-reconfigure.epub");
    let mut child = spawn_finalize_stage_delay_mock_translate(
        &temp,
        &events,
        &output,
        &[("BOOKFORGE_MOCK_QA_DELAY_MS", "3000")],
    );
    let job_id = wait_for_job_id_in_events(&events, &mut child);
    wait_for_child_events(&events, &mut child, |events| {
        request_started_ids(events)
            .iter()
            .any(|id| id.starts_with("qa_"))
    });
    // Freeze the later stage boundary instead of requiring a separate
    // reconfigure process to finish inside the QA provider's delay window.
    // The already-started QA request keeps revision 0; the paused boundary
    // cannot dispatch double-check until revision 1 is acknowledged below.
    write_control_file(&control_path(&temp, &job_id), ControlCommand::Pause)
        .expect("pause control should write");
    wait_for_child_job_status(&temp, &job_id, "paused", &mut child);

    bookforge()
        .current_dir(temp.path())
        .args([
            "reconfigure",
            &job_id,
            "--qa",
            "off",
            "--double-check",
            "off",
            "--validate-output",
            "true",
            "--provider-max-attempts",
            "4",
        ])
        .assert()
        .success();
    wait_for_child_events(&events, &mut child, |events| {
        event_count(events, "RuntimeConfigChanged") >= 1
    });
    bookforge()
        .current_dir(temp.path())
        .args(["resume", &job_id, "--ui", "quiet"])
        .assert()
        .success();

    let status = child.wait().expect("translate child should exit");
    assert!(status.success(), "translate child failed: {status}");
    let final_events = wait_for_event_count(&events, "TranslationFinished", 1);
    let qa_started = final_events
        .iter()
        .position(|event| {
            request_started_id(event)
                .as_deref()
                .is_some_and(|id| id.starts_with("qa_"))
        })
        .expect("QA request should start under the baseline stage snapshot");
    let qa_request = &final_events[qa_started]["RequestStarted"];
    assert_eq!(
        qa_request["runtime_config_revision"].as_u64(),
        Some(0),
        "the already-started QA stage must retain the baseline revision"
    );
    assert_eq!(
        qa_request["provider_max_attempts"].as_u64(),
        Some(1),
        "the already-started QA stage must retain the v1-fast attempt budget"
    );
    let changed = final_events
        .iter()
        .position(|event| event.get("RuntimeConfigChanged").is_some())
        .expect("runtime change should be recorded");
    let qa_finished = final_events
        .iter()
        .position(|event| {
            event
                .get("RequestFinished")
                .and_then(|payload| payload.get("request_id"))
                .and_then(serde_json::Value::as_str)
                .is_some_and(|id| id.starts_with("qa_"))
        })
        .expect("in-flight QA request should finish");
    // The override watcher and the in-flight provider request are concurrent
    // actors. The durable edit follows RequestStarted, but the watcher is not
    // required to emit RuntimeConfigChanged before RequestFinished. Assert only
    // the causal order plus the stage snapshots that production guarantees.
    assert!(
        qa_started < changed,
        "the runtime change cannot precede the request that triggered the edit"
    );
    assert!(
        qa_started < qa_finished,
        "the QA request must finish after it starts"
    );
    assert!(
        request_started_ids(&final_events)
            .iter()
            .all(|id| !id.starts_with("double_check_") && !id.starts_with("repair_")),
        "double-check disabled at its later stage boundary must not dispatch"
    );
    assert!(
        temp.path()
            .join("stage-live-reconfigure.validation.json")
            .exists(),
        "validation enabled at its stage boundary should write a report"
    );
    assert!(!overrides_path(&temp, &job_id).exists());
    assert!(!runtime_path(&temp, &job_id).exists());
}

#[test]
fn cli_stop_then_resume_mock_run() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let events = temp.path().join("events.jsonl");
    let output = temp.path().join("out.epub");
    let release = temp.path().join("mock-release");
    let mut child = spawn_gated_mock_translate(&temp, &events, &output, &release);
    let job_id = wait_for_job_id_in_events(&events, &mut child);
    let control_path = temp
        .path()
        .join(".bookforge/runs")
        .join(&job_id)
        .join("control");
    // Stop only after the first provider request is observably in flight. The
    // former fixed 50 ms sleep could fire before dispatch on a fast machine,
    // leaving no completed request to checkpoint. Stop still lets that
    // in-flight mock request finish while preventing the next dispatch.
    //
    // The gate is what makes "in flight" hold still: waiting on RequestStarted
    // alone leaves a window where request 1 finishes and request 2 dispatches
    // before the stop lands, which breaks the one-request assertion below.
    // Parking request 1 until the stop is written closes that window, and the
    // release then lets it finish so there is a checkpointed segment to resume.
    wait_for_first_batch_request(&events, &mut child);
    write_control_file(&control_path, ControlCommand::Stop).expect("stop control should write");
    fs::write(&release, "release").expect("mock release file should write");

    let status = child.wait().expect("translate child should exit");
    assert!(status.success(), "translate child failed: {status}");
    wait_for_job_status(&temp, &job_id, "stopped");
    let stopped_events = read_jsonl(&events);
    assert_eq!(
        event_count(&stopped_events, "TranslationFinished"),
        0,
        "stopped run should not emit final completion"
    );
    let initially_finished = segment_finished_ids(&stopped_events);
    assert!(
        !initially_finished.is_empty(),
        "stop test should checkpoint at least one segment"
    );
    assert_eq!(
        batch_request_started_count(&stopped_events),
        1,
        "batch stop should not start another provider request after first completion"
    );
    let store = JobStore::open(temp.path().join(".bookforge/jobs.sqlite")).expect("store opens");
    let summary = store
        .summary(&job_id)
        .expect("summary should load")
        .expect("job should exist");
    assert_eq!(
        summary.failed, 0,
        "stopped batch items should remain resumable instead of failed"
    );

    let resume_events = temp.path().join("resume-events.jsonl");
    bookforge()
        .current_dir(temp.path())
        .args([
            "resume",
            &job_id,
            "--ui",
            "quiet",
            "--progress-jsonl",
            resume_events.to_str().unwrap(),
        ])
        .assert()
        .success();

    let resumed_events = wait_for_event_count(&resume_events, "TranslationFinished", 1);
    let resumed_finished = segment_finished_ids(&resumed_events);
    assert!(
        !resumed_finished.is_empty(),
        "resume should translate remaining segments"
    );
    for id in &initially_finished {
        assert!(
            !resumed_finished.contains(id),
            "resume retranslated already checkpointed segment {id}"
        );
    }
    assert!(output.exists(), "resume after stop should write output");
}

#[test]
fn cli_pause_during_inflight_batch_holds_finalize_passes() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let events = temp.path().join("finalize-pause-events.jsonl");
    let output = temp.path().join("finalize-pause.epub");
    let release = temp.path().join("finalize-pause-release");
    let mut child = spawn_finalize_stage_delay_mock_translate(
        &temp,
        &events,
        &output,
        &[(
            "BOOKFORGE_MOCK_RELEASE_FILE",
            release.to_str().expect("release path should be UTF-8"),
        )],
    );
    let job_id = wait_for_job_id_in_events(&events, &mut child);
    wait_for_first_batch_request(&events, &mut child);

    let control_path = temp
        .path()
        .join(".bookforge/runs")
        .join(&job_id)
        .join("control");
    write_control_file(&control_path, ControlCommand::Pause).expect("pause control should write");
    wait_for_child_job_status(&temp, &job_id, "paused", &mut child);
    fs::write(&release, "release").expect("mock release file should write");
    wait_for_child_events(&events, &mut child, |events| {
        batch_request_finished_count(events) >= 1
    });

    let paused_events = read_jsonl(&events);
    assert!(
        finalize_request_started_between_pause_and_resume(&paused_events).is_empty(),
        "finalize provider requests started while paused: {:?}",
        finalize_request_started_between_pause_and_resume(&paused_events)
    );
    assert_eq!(
        event_count(&paused_events, "TranslationFinished"),
        0,
        "paused finalize run should not complete"
    );

    bookforge()
        .current_dir(temp.path())
        .args(["resume", &job_id, "--ui", "quiet"])
        .assert()
        .success();

    let status = child.wait().expect("translate child should exit");
    assert!(status.success(), "translate child failed: {status}");
    let final_events = wait_for_event_count(&events, "TranslationFinished", 1);
    assert!(
        finalize_request_started_count(&final_events) > 0,
        "resumed run should execute QA/double-check finalize provider requests"
    );
    assert!(
        finalize_request_started_ids(&final_events)
            .iter()
            .any(|id| id.starts_with("repair_") || id.starts_with("double_check_")),
        "forced double-check/repair pass should run after resume; ids={:?}",
        finalize_request_started_ids(&final_events)
    );
    assert!(output.exists(), "resumed finalize run should write output");
}

#[test]
fn cli_stop_during_inflight_batch_skips_finalize_until_resume() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let events = temp.path().join("finalize-stop-events.jsonl");
    let output = temp.path().join("finalize-stop.epub");
    let release = temp.path().join("finalize-stop-release");
    let mut child = spawn_finalize_stage_delay_mock_translate(
        &temp,
        &events,
        &output,
        &[(
            "BOOKFORGE_MOCK_RELEASE_FILE",
            release.to_str().expect("release path should be UTF-8"),
        )],
    );
    let job_id = wait_for_job_id_in_events(&events, &mut child);
    wait_for_first_batch_request(&events, &mut child);

    let control_path = temp
        .path()
        .join(".bookforge/runs")
        .join(&job_id)
        .join("control");
    write_control_file(&control_path, ControlCommand::Stop).expect("stop control should write");
    fs::write(&release, "release").expect("mock release file should write");

    let status = child.wait().expect("translate child should exit");
    assert!(status.success(), "translate child failed: {status}");
    assert_eq!(job_status(&temp, &job_id), "stopped");
    let stopped_events = read_jsonl(&events);
    assert_eq!(
        finalize_request_started_count(&stopped_events),
        0,
        "stopped run should not start finalize provider requests"
    );
    assert_eq!(
        event_count(&stopped_events, "TranslationFinished"),
        0,
        "stopped run should not emit final completion"
    );

    let resume_events = temp.path().join("finalize-stop-resume-events.jsonl");
    bookforge()
        .current_dir(temp.path())
        .env("BOOKFORGE_MOCK_DOUBLE_CHECK_FAIL", "1")
        .args([
            "resume",
            &job_id,
            "--ui",
            "quiet",
            "--progress-jsonl",
            resume_events.to_str().unwrap(),
        ])
        .assert()
        .success();

    let resumed_events = wait_for_event_count(&resume_events, "TranslationFinished", 1);
    assert!(
        finalize_request_started_count(&resumed_events) > 0,
        "resume should execute skipped finalize provider requests"
    );
    assert!(
        finalize_request_started_ids(&resumed_events)
            .iter()
            .any(|id| id.starts_with("repair_") || id.starts_with("double_check_")),
        "resume should run forced double-check/repair pass; ids={:?}",
        finalize_request_started_ids(&resumed_events)
    );
    assert!(
        output.exists(),
        "resume after finalize stop should write output"
    );
}

#[test]
fn cli_pause_during_inflight_qa_request_parks_before_more_finalize_work() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let events = temp.path().join("finalize-qa-pause-events.jsonl");
    let output = temp.path().join("finalize-qa-pause.epub");
    let mut child = spawn_finalize_stage_delay_mock_translate(
        &temp,
        &events,
        &output,
        &[("BOOKFORGE_MOCK_QA_DELAY_MS", "3000")],
    );
    let job_id = wait_for_job_id_in_events(&events, &mut child);
    wait_for_child_events(&events, &mut child, |events| {
        request_started_ids(events)
            .iter()
            .any(|id| id.starts_with("qa_"))
    });

    let control_path = control_path(&temp, &job_id);
    write_control_file(&control_path, ControlCommand::Pause).expect("pause control should write");
    wait_for_child_job_status(&temp, &job_id, "paused", &mut child);
    // RequestStarted precedes the provider call, so Pause may park the task
    // before it can emit RequestFinished. Observe beyond the injected provider
    // delay, then assert that no later finalize work escaped the paused state.
    thread::sleep(Duration::from_millis(3300));

    let paused_events = read_jsonl(&events);
    assert!(
        finalize_request_started_between_pause_and_resume(&paused_events).is_empty(),
        "new finalize requests started while QA pause was parked: {:?}",
        finalize_request_started_between_pause_and_resume(&paused_events)
    );
    assert_eq!(
        event_count(&paused_events, "TranslationFinished"),
        0,
        "paused in-flight QA run should not complete"
    );

    bookforge()
        .current_dir(temp.path())
        .args(["resume", &job_id, "--ui", "quiet"])
        .assert()
        .success();

    let status = child.wait().expect("translate child should exit");
    assert!(status.success(), "translate child failed: {status}");
    wait_for_event_count(&events, "TranslationFinished", 1);
    assert!(output.exists(), "resumed QA pause run should write output");
}

#[test]
fn cli_pause_during_inflight_double_check_request_parks_before_corrections() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let events = temp.path().join("finalize-double-check-pause-events.jsonl");
    let output = temp.path().join("finalize-double-check-pause.epub");
    let mut child = spawn_finalize_stage_delay_mock_translate(
        &temp,
        &events,
        &output,
        &[("BOOKFORGE_MOCK_DOUBLE_CHECK_DELAY_MS", "3000")],
    );
    let job_id = wait_for_job_id_in_events(&events, &mut child);
    wait_for_child_events(&events, &mut child, |events| {
        request_started_ids(events)
            .iter()
            .any(|id| id.starts_with("double_check_"))
    });

    let control_path = control_path(&temp, &job_id);
    write_control_file(&control_path, ControlCommand::Pause).expect("pause control should write");
    wait_for_child_job_status(&temp, &job_id, "paused", &mut child);
    // As above, the request may be parked between RequestStarted and the
    // provider call, in which case RequestFinished correctly cannot appear.
    thread::sleep(Duration::from_millis(3300));

    let paused_events = read_jsonl(&events);
    assert!(
        finalize_request_started_between_pause_and_resume(&paused_events)
            .iter()
            .all(|id| !id.starts_with("repair_")),
        "correction requests started while double-check pause was parked: {:?}",
        finalize_request_started_between_pause_and_resume(&paused_events)
    );
    assert_eq!(
        event_count(&paused_events, "TranslationFinished"),
        0,
        "paused in-flight double-check run should not complete"
    );

    bookforge()
        .current_dir(temp.path())
        .args(["resume", &job_id, "--ui", "quiet"])
        .assert()
        .success();

    let status = child.wait().expect("translate child should exit");
    assert!(status.success(), "translate child failed: {status}");
    wait_for_event_count(&events, "TranslationFinished", 1);
    assert!(
        output.exists(),
        "resumed double-check pause run should write output"
    );
}

#[test]
fn cli_stop_during_finalize_resume_runs_fallback_and_marks_terminal_status() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let events = temp.path().join("finalize-fallback-stop-events.jsonl");
    let output = temp.path().join("finalize-fallback-stop.epub");
    let mut child = spawn_finalize_fallback_mock_translate(&temp, &events, &output);
    let job_id = wait_for_job_id_in_events(&events, &mut child);
    // Determinism: stop mid-flight, while the primary pass is still working,
    // rather than racing the finalize/fallback boundary. The recovery
    // expectations below stay identical either way: resume must replay the
    // remaining fallback pass for any segments the stopped run never rescued.
    wait_for_child_events(&events, &mut child, |events| {
        event_count(events, "RequestStarted") >= 1 && event_count(events, "SegmentFinished") >= 1
    });

    write_control_file(&control_path(&temp, &job_id), ControlCommand::Stop)
        .expect("stop control should write");
    let status = child.wait().expect("translate child should exit");
    assert!(status.success(), "translate child failed: {status}");
    wait_for_job_status(&temp, &job_id, "stopped");
    assert_eq!(
        event_count(&read_jsonl(&events), "TranslationFinished"),
        0,
        "stopped finalize run should not emit completion"
    );

    let resume_events = temp.path().join("finalize-fallback-resume-events.jsonl");
    bookforge()
        .current_dir(temp.path())
        .args([
            "resume",
            &job_id,
            "--ui",
            "quiet",
            "--progress-jsonl",
            resume_events.to_str().unwrap(),
        ])
        .assert()
        .success();

    let resumed_events = wait_for_event_count(&resume_events, "TranslationFinished", 1);
    let request_ids = request_started_ids(&resumed_events);
    // Which work the resume has to redo depends on where the stopped run
    // actually froze: if Stop landed BEFORE its fallback pass rescued the
    // flagged segments, resume replays fallback (fallback_ requests appear);
    // if it landed AFTER everything was already checkpointed as succeeded,
    // resume simply finalizes without extra provider calls. Both outcomes are
    // truthful; assert the one matching the state left behind.
    let phase1_rescued_everything = {
        let store =
            JobStore::open(temp.path().join(".bookforge/jobs.sqlite")).expect("store opens");
        let records = store
            .segment_records(&job_id)
            .expect("segment records load");
        !records.is_empty()
            && records
                .iter()
                .all(|record| matches!(record.status.as_str(), "succeeded" | "skipped_cached"))
    };
    assert!(
        request_ids.iter().any(|id| id.starts_with("fallback_")) || phase1_rescued_everything,
        "resume must either replay the fallback pass or finalize an already-rescued book; \
         ids={request_ids:?} rescued_all={phase1_rescued_everything}"
    );
    assert_eq!(job_status(&temp, &job_id), "succeeded");
    assert!(
        output.exists(),
        "resume after fallback stop should write output"
    );
}

#[test]
fn cli_resume_after_stop_with_persisted_corrections_is_idempotent() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let events = temp.path().join("finalize-correction-stop-events.jsonl");
    let output = temp.path().join("finalize-correction-stop.epub");
    let mut child = spawn_finalize_stage_delay_mock_translate(
        &temp,
        &events,
        &output,
        // Correction persistence happens after RequestFinished and before the
        // next controllable stage boundary. This existing test-only injection
        // keeps that production-default-free window open long enough to issue
        // Stop; child-aware waits below still fail immediately on early exit.
        &[("BOOKFORGE_TEST_FINALIZE_BOUNDARY_DELAY_MS", "3000")],
    );
    let job_id = wait_for_job_id_in_events(&events, &mut child);
    wait_for_child_events(&events, &mut child, |events| {
        request_finished_ids(events)
            .iter()
            .any(|id| id.starts_with("repair_"))
    });
    wait_for_child_corrected_block(&temp, &job_id, &mut child);

    write_control_file(&control_path(&temp, &job_id), ControlCommand::Stop)
        .expect("stop control should write");
    let status = child.wait().expect("translate child should exit");
    assert!(status.success(), "translate child failed: {status}");
    assert_eq!(job_status(&temp, &job_id), "stopped");

    let resume_events = temp.path().join("finalize-correction-resume-events.jsonl");
    bookforge()
        .current_dir(temp.path())
        .env("BOOKFORGE_MOCK_DOUBLE_CHECK_FAIL", "1")
        .args([
            "resume",
            &job_id,
            "--ui",
            "quiet",
            "--progress-jsonl",
            resume_events.to_str().unwrap(),
        ])
        .assert()
        .success();

    wait_for_event_count(&resume_events, "TranslationFinished", 1);
    let block_texts = stored_block_texts(&temp, &job_id);
    assert!(
        block_texts
            .iter()
            .all(|text| !text.contains("[corrected] [corrected]")),
        "resume double-applied persisted corrections: {block_texts:?}"
    );
    assert!(
        request_started_ids(&read_jsonl(&resume_events))
            .iter()
            .all(|id| !id.starts_with("double_check_") && !id.starts_with("repair_")),
        "resume should skip checkpointed double-check pass"
    );
    assert_eq!(job_status(&temp, &job_id), "succeeded");
    assert!(
        output.exists(),
        "resume after correction stop should write output"
    );
}

#[test]
fn cli_resume_uses_input_snapshot_after_original_is_moved() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let fixture = fixture_input();
    let input = temp.path().join("source.epub");
    let moved = temp.path().join("source-moved.epub");
    fs::copy(&fixture, &input).expect("fixture should copy");
    let run = translate_quiet_input(&temp, &input, "mock-prefix-target");

    let snapshot = temp
        .path()
        .join(".bookforge/runs")
        .join(&run.job_id)
        .join("input.epub");
    let snapshot_sha = temp
        .path()
        .join(".bookforge/runs")
        .join(&run.job_id)
        .join("input.sha256");
    assert!(snapshot.exists(), "input snapshot should exist");
    assert_eq!(
        fs::read_to_string(&snapshot_sha)
            .expect("snapshot sha should read")
            .trim(),
        sha256_file(&snapshot)
    );

    let store = JobStore::open(temp.path().join(".bookforge/jobs.sqlite")).expect("store opens");
    let retry_id = store
        .segment_records(&run.job_id)
        .expect("segments should load")
        .into_iter()
        .next()
        .expect("fixture should have a segment")
        .id;
    store
        .mark_segment_failed(&run.job_id, &retry_id, "force resume")
        .expect("segment should mark failed");
    drop(store);
    fs::rename(&input, &moved).expect("input should move");

    bookforge()
        .current_dir(temp.path())
        .args(["resume", &run.job_id, "--ui", "quiet"])
        .assert()
        .success();
}

// --- Dashboard-driven single-segment retry with guidance ---------------
//
// `JobStore::request_segment_retry` stores optional guidance text in
// `segment_flags` (kind = 'dashboard_retry') and marks the segment /job
// `retry_pending`. `resume` reloads that guidance fresh via
// `JobStore::load_retry_guidance` (see commands/resume.rs) and wires it into
// `TranslationRunConfig.guidance_by_segment`, which is rendered into the
// prompt (`prompt_extra_for_segment` in single-segment mode,
// `render_batch_items`'s `retry_guidance` field in batch mode) and is
// consumed only once a terminal provider result is saved
// (`consume_dashboard_retry_guidance`, called from `save_translation`,
// `save_needs_review`, and `save_cached_translation`).
//
// IMPORTANT LIMITATION: the compiled `mock` provider used by these
// subprocess-driven lifecycle tests (`bookforge_llm::provider::MockProvider`)
// never observes or logs the rendered `request.user` prompt text it
// receives — it only ever echoes back a transform of the *source* text, and
// nothing persists the raw prompt anywhere a subprocess-based integration
// test can read it back (no db column, no progress-jsonl event field, no
// env-var-gated dump). So these lifecycle tests cannot assert "the prompt
// text contains the guidance string" the way `bookforge-llm`'s in-process
// unit tests can (see `prompt_renders_glossary_json_prose_and_prompt_extra`
// in crates/bookforge-llm/src/scheduler.rs and the analogous
// `render_batch_items`-guidance test in crates/bookforge-llm/src/batch.rs,
// which assert the rendered prompt/JSON item literally contains
// `retry_guidance`/the guidance text). At the lifecycle level we instead
// assert the strongest observable proxy: guidance is present in the store
// before resume, the flagged segment is provably re-translated by resume
// (not served from cache, not skipped), and guidance is gone afterward.
// A production hook that would make the prompt text itself observable here
// would be an env-var-gated capture in `MockProvider::complete` (e.g.
// `BOOKFORGE_MOCK_PROMPT_LOG=<path>` appending
// `{request_id/segment_id, template, user}` as JSONL) — deliberately not
// added here since this pass is test-only.

#[test]
fn cli_resume_after_dashboard_retry_guidance_retranslates_single_segment_mode() {
    // `--profile safe` disables batching, so this exercises the
    // single-segment prompt path (`prompt_extra_for_segment`) for guidance.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let input = fixture_input();
    let output = temp.path().join("out.epub");
    let events = temp.path().join("events.jsonl");
    bookforge()
        .current_dir(temp.path())
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
            "safe",
            "--ui",
            "quiet",
            "--progress-jsonl",
            events.to_str().unwrap(),
            "--out",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();
    let job_id = job_id_from_events(&events);

    let db_path = temp.path().join(".bookforge/jobs.sqlite");
    let store = JobStore::open(&db_path).expect("store should open");
    let before = store
        .segment_records(&job_id)
        .expect("segments should load");
    assert!(!before.is_empty(), "fixture should produce segments");
    let retry_id = before[0].id.clone();
    let attempts_before = before[0].attempts;

    store
        .request_segment_retry(
            &job_id,
            &retry_id,
            Some("Use a more formal register for this paragraph."),
        )
        .expect("retry request should succeed");
    let guidance_before_resume = store
        .load_retry_guidance(&job_id)
        .expect("guidance should load");
    assert_eq!(
        guidance_before_resume
            .get(retry_id.as_str())
            .map(String::as_str),
        Some("Use a more formal register for this paragraph."),
        "guidance stored by request_segment_retry should be readable back from the store \
         (i.e. it survives independent of any in-process state)"
    );
    // Drop this handle and open a brand-new one after `resume` runs as a
    // fresh subprocess below, so nothing about this test relies on shared
    // in-memory state surviving a "restart".
    drop(store);

    let resume_events = temp.path().join("resume-events.jsonl");
    bookforge()
        .current_dir(temp.path())
        .args([
            "resume",
            &job_id,
            "--ui",
            "quiet",
            "--progress-jsonl",
            resume_events.to_str().unwrap(),
        ])
        .assert()
        .success();

    let events_after = read_jsonl(&resume_events);
    assert_eq!(
        batch_request_started_count(&events_after),
        0,
        "safe profile should stay on the single-segment prompt path"
    );
    assert_eq!(
        segment_finished_ids(&events_after),
        vec![retry_id.clone()],
        "resume should retranslate exactly the segment flagged via dashboard retry"
    );

    let store = JobStore::open(&db_path).expect("store should open");
    let after = store
        .segment_records(&job_id)
        .expect("segments should load after resume");
    let retried = after
        .iter()
        .find(|record| record.id == retry_id)
        .expect("retried segment should still be present");
    assert_eq!(
        retried.status, "succeeded",
        "retried segment should reach a terminal status"
    );
    assert!(
        retried.attempts > attempts_before,
        "retried segment should have actually been re-translated (attempts increased from {attempts_before} to {})",
        retried.attempts
    );

    let guidance_after_resume = store
        .load_retry_guidance(&job_id)
        .expect("guidance should load after resume");
    assert!(
        !guidance_after_resume.contains_key(retry_id.as_str()),
        "guidance should be consumed once the retried segment reaches a terminal (succeeded) result"
    );
}

#[test]
fn cli_resume_after_dashboard_retry_guidance_retranslates_batch_mode() {
    // Default profile (v1-fast) keeps batching enabled, so this exercises
    // the batch prompt path where guidance is serialized per-item as
    // `retry_guidance` by `render_batch_items` (bookforge-llm/src/batch.rs).
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let run = translate_quiet(&temp, "mock-prefix-target");

    let db_path = temp.path().join(".bookforge/jobs.sqlite");
    let store = JobStore::open(&db_path).expect("store should open");
    let before = store
        .segment_records(&run.job_id)
        .expect("segments should load");
    assert!(!before.is_empty(), "fixture should produce segments");
    let retry_id = before[0].id.clone();
    let attempts_before = before[0].attempts;

    store
        .request_segment_retry(
            &run.job_id,
            &retry_id,
            Some("Tighten the dialogue tag here."),
        )
        .expect("retry request should succeed");
    assert_eq!(
        store
            .load_retry_guidance(&run.job_id)
            .expect("guidance should load")
            .get(retry_id.as_str())
            .map(String::as_str),
        Some("Tighten the dialogue tag here.")
    );
    drop(store);

    let resume_events = temp.path().join("resume-events.jsonl");
    bookforge()
        .current_dir(temp.path())
        .args([
            "resume",
            &run.job_id,
            "--ui",
            "quiet",
            "--progress-jsonl",
            resume_events.to_str().unwrap(),
        ])
        .assert()
        .success();

    let events_after = read_jsonl(&resume_events);
    assert!(
        batch_request_started_count(&events_after) >= 1,
        "v1-fast profile should route the retry through the batch prompt path"
    );
    assert_eq!(
        segment_finished_ids(&events_after),
        vec![retry_id.clone()],
        "resume should retranslate exactly the segment flagged via dashboard retry"
    );

    let store = JobStore::open(&db_path).expect("store should open");
    let after = store
        .segment_records(&run.job_id)
        .expect("segments should load after resume");
    let retried = after
        .iter()
        .find(|record| record.id == retry_id)
        .expect("retried segment should still be present");
    assert_eq!(retried.status, "succeeded");
    assert!(
        retried.attempts > attempts_before,
        "retried segment should have actually been re-translated (attempts increased from {attempts_before} to {})",
        retried.attempts
    );
    assert!(
        !store
            .load_retry_guidance(&run.job_id)
            .expect("guidance should load after resume")
            .contains_key(retry_id.as_str()),
        "guidance should be consumed once the retried segment reaches a terminal (succeeded) result in batch mode too"
    );
}

#[test]
fn cli_request_segment_retry_rejects_segment_frozen_by_correct_command() {
    // Store-level coverage for this rejection already exists (see
    // `request_segment_retry_rejects_human_corrected_segment` in
    // crates/bookforge-store/src/db.rs). This lifecycle-level test is cheap
    // to add on top of the existing `correct`-flow fixture and exercises the
    // rejection through the real `correct` CLI command (which calls
    // `save_manual_correction`) rather than a direct store call, proving the
    // freeze is honored end-to-end.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let run = translate_quiet(&temp, "mock-prefix-target");
    let store = JobStore::open(temp.path().join(".bookforge/jobs.sqlite")).expect("store opens");
    let segment = store
        .load_terminal_segment_translations(&run.job_id)
        .expect("translations should load")
        .into_iter()
        .next()
        .expect("fixture should produce a translated segment");
    let corrected_blocks = segment
        .blocks
        .iter()
        .map(|block| {
            serde_json::json!({
                "block_id": block.block_id.0,
                "text": format!("MANUAL {}", block.text),
            })
        })
        .collect::<Vec<_>>();
    let correction_path = temp.path().join("correction.json");
    fs::write(
        &correction_path,
        serde_json::to_vec_pretty(&serde_json::json!({ "blocks": corrected_blocks }))
            .expect("correction should serialize"),
    )
    .expect("correction file should write");

    bookforge()
        .current_dir(temp.path())
        .args([
            "correct",
            &run.job_id,
            "--segment",
            &segment.segment_id,
            "--from-file",
            correction_path.to_str().unwrap(),
        ])
        .assert()
        .success();

    let result = store.request_segment_retry(&run.job_id, &segment.segment_id, Some("try again"));
    assert!(
        matches!(result, Err(StoreError::InvalidCorrection(_))),
        "retrying a segment frozen by a human correction must be rejected, got: {result:?}"
    );

    let guidance = store
        .load_retry_guidance(&run.job_id)
        .expect("guidance should load");
    assert!(
        !guidance.contains_key(segment.segment_id.as_str()),
        "rejected retry must not record guidance for the frozen segment"
    );
    let records = store
        .segment_records(&run.job_id)
        .expect("segment records should load");
    let frozen = records
        .iter()
        .find(|record| record.id == segment.segment_id)
        .expect("frozen segment should remain present");
    assert_eq!(
        frozen.status, "succeeded",
        "rejected retry must not disturb the frozen segment's status"
    );
}

#[test]
fn cli_review_generates_artifacts_and_ingest_flags_marks_retry() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let run = translate_quiet(&temp, "mock-prefix-target");
    let review_dir = temp.path().join("review-out");

    bookforge()
        .current_dir(temp.path())
        .args(["review", &run.job_id, "--out", review_dir.to_str().unwrap()])
        .assert()
        .success();

    let review_json_path = review_dir.join("review.json");
    let review_html_path = review_dir.join("index.html");
    assert!(review_json_path.exists(), "review.json should exist");
    assert!(review_html_path.exists(), "index.html should exist");
    let review_json: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&review_json_path).expect("review JSON should read"),
    )
    .expect("review JSON should parse");
    let segments = review_json
        .get("segments")
        .and_then(|value| value.as_array())
        .expect("segments array should exist");
    assert!(!segments.is_empty(), "review should contain segments");
    let sum_input = segments
        .iter()
        .map(|segment| {
            segment
                .pointer("/tokens/input")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
        })
        .sum::<u64>();
    let total_input = review_json
        .pointer("/totals/tokens_input")
        .and_then(|value| value.as_u64())
        .expect("total input tokens should exist");
    assert_eq!(total_input, sum_input);
    assert!(
        fs::read_to_string(&review_html_path)
            .expect("review HTML should read")
            .contains("This page contains the full text of your book. Treat as private.")
    );

    let first_segment = segments[0]
        .get("segment_id")
        .and_then(|value| value.as_str())
        .expect("segment id should exist");
    let flags_path = temp.path().join("flags.json");
    fs::write(
        &flags_path,
        serde_json::to_string_pretty(&serde_json::json!({
            "schema_version": 1,
            "job_id": run.job_id.clone(),
            "exported_at": "2026-05-06T13:45:00Z",
            "flags": [{
                "segment_id": first_segment,
                "kind": "wrong_translation",
                "note": "Meaning is reversed.",
                "suggested_source": null,
                "suggested_target": null
            }]
        }))
        .unwrap(),
    )
    .expect("flags should write");

    bookforge()
        .current_dir(temp.path())
        .args([
            "ingest-flags",
            &run.job_id,
            "--flags",
            flags_path.to_str().unwrap(),
        ])
        .assert()
        .success();

    let store = JobStore::open(temp.path().join(".bookforge/jobs.sqlite")).expect("store opens");
    assert_eq!(
        store
            .segment_flag_count(&run.job_id)
            .expect("flag count should load"),
        1
    );
    let summary = store
        .summary(&run.job_id)
        .expect("summary should load")
        .expect("job should exist");
    assert_eq!(summary.needs_review, 1);
    drop(store);

    bookforge()
        .current_dir(temp.path())
        .args(["retry", &run.job_id, "--only", "needs-review"])
        .assert()
        .success();

    let store = JobStore::open(temp.path().join(".bookforge/jobs.sqlite")).expect("store opens");
    let summary = store
        .summary(&run.job_id)
        .expect("summary should load")
        .expect("job should exist");
    assert_eq!(summary.retry_pending, 1);
}

struct TranslateRun {
    job_id: String,
    output: PathBuf,
    events: PathBuf,
    report: PathBuf,
}

fn translate_quiet(temp: &TempDir, model: &str) -> TranslateRun {
    let input = fixture_input();
    translate_quiet_input(temp, &input, model)
}

fn translate_quiet_input(temp: &TempDir, input: &Path, model: &str) -> TranslateRun {
    let output = temp.path().join("out.epub");
    let events = temp.path().join("events.jsonl");
    let assert = bookforge()
        .current_dir(temp.path())
        .args([
            "translate",
            input.to_str().unwrap(),
            "--target",
            "Italian",
            "--provider",
            "mock",
            "--model",
            model,
            "--profile",
            "v1-fast",
            "--ui",
            "quiet",
            "--progress-jsonl",
            events.to_str().unwrap(),
            "--out",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();
    assert!(
        assert.get_output().stdout.is_empty(),
        "translate --ui quiet should not write human stdout"
    );
    let job_id = job_id_from_events(&events);

    TranslateRun {
        job_id,
        report: temp.path().join("out.report.md"),
        output,
        events,
    }
}

/// A spawned CLI child that is killed if the test unwinds before reaping it.
///
/// Pause has no timeout — a paused job waits indefinitely, which is the feature
/// — so a test that panics between "pause" and "resume/stop" leaves a parked
/// `bookforge` process behind for as long as the machine stays up. This guard
/// only cleans up after a *failing* test. It deliberately does not stand in for
/// the product behaviour: that a parked run can still be terminated in-band is
/// asserted by `cli_stop_terminates_a_parked_paused_run`, so a regression there
/// fails loudly instead of being quietly reaped from outside.
struct ChildGuard(process::Child);

impl std::ops::Deref for ChildGuard {
    type Target = process::Child;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for ChildGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn spawn_controlled_mock_translate(temp: &TempDir, events: &Path, output: &Path) -> ChildGuard {
    spawn_controlled_mock_translate_inner(temp, events, output, None)
}

/// The same run as [`spawn_controlled_mock_translate`], except every mock
/// provider request parks until `release` exists. Tests that must drive a
/// control-file transition while a request is in flight use this instead of
/// racing `BOOKFORGE_MOCK_DELAY_MS`: they wait for `RequestStarted`, do their
/// setup, then create `release` to let the request finish.
fn spawn_gated_mock_translate(
    temp: &TempDir,
    events: &Path,
    output: &Path,
    release: &Path,
) -> ChildGuard {
    spawn_controlled_mock_translate_inner(temp, events, output, Some(release))
}

fn spawn_controlled_mock_translate_inner(
    temp: &TempDir,
    events: &Path,
    output: &Path,
    release: Option<&Path>,
) -> ChildGuard {
    let input = fixture_input();
    let mut cmd = process::Command::new(assert_cmd::cargo::cargo_bin("bookforge"));
    cmd.current_dir(temp.path())
        .env("BOOKFORGE_MOCK_DELAY_MS", "300")
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
            "--max-segment-tokens",
            "1",
            "--batch-max-items",
            "1",
            "--context-window",
            "0",
            "--concurrency",
            "1",
            "--ui",
            "quiet",
            "--progress-jsonl",
            events.to_str().unwrap(),
            "--out",
            output.to_str().unwrap(),
        ]);
    if let Some(release) = release {
        cmd.env("BOOKFORGE_MOCK_RELEASE_FILE", release);
    }
    ChildGuard(cmd.spawn().expect("controlled translate should spawn"))
}

/// Start a single-request run with its first provider request held at the mock
/// release gate. The reconfigure test releases it only after the sidecar update
/// has landed, so request 1 keeps the baseline revision and later requests see
/// the new revision regardless of host scheduling.
fn spawn_gated_single_mock_translate(
    temp: &TempDir,
    events: &Path,
    output: &Path,
    release: &Path,
) -> ChildGuard {
    let input = fixture_input();
    let mut cmd = process::Command::new(assert_cmd::cargo::cargo_bin("bookforge"));
    cmd.current_dir(temp.path())
        .env("BOOKFORGE_MOCK_DELAY_MS", "400")
        .env("BOOKFORGE_MOCK_RELEASE_FILE", release)
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
            "safe",
            "--max-segment-tokens",
            "1",
            "--context-window",
            "0",
            "--concurrency",
            "1",
            "--ui",
            "quiet",
            "--progress-jsonl",
            events.to_str().unwrap(),
            "--out",
            output.to_str().unwrap(),
        ]);
    ChildGuard(
        cmd.spawn()
            .expect("controlled single-segment translate should spawn"),
    )
}

fn spawn_finalize_stage_delay_mock_translate(
    temp: &TempDir,
    events: &Path,
    output: &Path,
    envs: &[(&str, &str)],
) -> ChildGuard {
    let input = fixture_input();
    let mut cmd = process::Command::new(assert_cmd::cargo::cargo_bin("bookforge"));
    cmd.current_dir(temp.path())
        .env("BOOKFORGE_MOCK_DOUBLE_CHECK_FAIL", "1")
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
            "--batch-target-tokens",
            "100000",
            "--batch-max-items",
            "100",
            "--context-window",
            "0",
            "--concurrency",
            "1",
            "--qa",
            "all",
            "--qa-batch-target-tokens",
            "100000",
            "--double-check",
            "formatting",
            "--double-check-provider",
            "mock",
            "--double-check-model",
            "mock-prefix-target",
            "--double-check-batch-target-tokens",
            "100000",
            "--auto-correct",
            "--ui",
            "quiet",
            "--progress-jsonl",
            events.to_str().unwrap(),
            "--out",
            output.to_str().unwrap(),
        ]);
    for (name, value) in envs {
        cmd.env(name, value);
    }
    ChildGuard(
        cmd.spawn()
            .expect("finalize-controlled translate should spawn"),
    )
}

fn spawn_finalize_fallback_mock_translate(
    temp: &TempDir,
    events: &Path,
    output: &Path,
) -> ChildGuard {
    let input = fixture_input();
    let mut cmd = process::Command::new(assert_cmd::cargo::cargo_bin("bookforge"));
    cmd.current_dir(temp.path())
        .env("BOOKFORGE_TEST_FINALIZE_BOUNDARY_DELAY_MS", "1500")
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
            "--batch-target-tokens",
            "100000",
            "--batch-max-items",
            "100",
            "--context-window",
            "0",
            "--concurrency",
            "1",
            "--fallback-provider",
            "mock",
            "--fallback-model",
            "mock-prefix-target",
            "--fallback-only",
            "failed-and-needs-review",
            "--ui",
            "quiet",
            "--progress-jsonl",
            events.to_str().unwrap(),
            "--out",
            output.to_str().unwrap(),
        ]);
    ChildGuard(
        cmd.spawn()
            .expect("finalize-fallback translate should spawn"),
    )
}

fn sha256_file(path: &Path) -> String {
    let bytes = fs::read(path).expect("file should read");
    let digest = Sha256::digest(&bytes);
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing hash should not fail");
    }
    output
}

fn job_id_from_events(path: &Path) -> String {
    read_jsonl(path)
        .into_iter()
        .find_map(|event| {
            event
                .get("JobCreated")
                .and_then(|payload| payload.get("job_id"))
                .and_then(|value| value.as_str())
                .map(ToOwned::to_owned)
        })
        .expect("event log should include job id")
}

/// Wait for the child to announce the job it durably created.
///
/// The translate flow writes the SQLite job before emitting `JobCreated`, so
/// this event is the readiness handshake these tests need. Unlike the old
/// 60-second SQLite polling deadline, the outcome depends only on child state:
/// either the readiness event arrives, or the child exits and the test reports
/// that status. A runnable child is not failed merely because build load keeps
/// it off-CPU for an arbitrary wall-clock interval.
fn wait_for_job_id_in_events(path: &Path, child: &mut process::Child) -> String {
    let events =
        wait_for_child_events(path, child, |events| event_count(events, "JobCreated") >= 1);
    events
        .iter()
        .find_map(|event| {
            event
                .get("JobCreated")
                .and_then(|payload| payload.get("job_id"))
                .and_then(|value| value.as_str())
                .map(ToOwned::to_owned)
        })
        .expect("observed JobCreated event should include a job id")
}

fn control_path(temp: &TempDir, job_id: &str) -> PathBuf {
    temp.path()
        .join(".bookforge/runs")
        .join(job_id)
        .join("control")
}

fn overrides_path(temp: &TempDir, job_id: &str) -> PathBuf {
    temp.path()
        .join(".bookforge/runs")
        .join(job_id)
        .join("overrides.json")
}

fn runtime_path(temp: &TempDir, job_id: &str) -> PathBuf {
    temp.path()
        .join(".bookforge/runs")
        .join(job_id)
        .join("runtime.json")
}

fn wait_for_event_count(path: &Path, key: &str, min_count: usize) -> Vec<serde_json::Value> {
    wait_for_events(path, |events| event_count(events, key) >= min_count)
}

fn wait_for_events(
    path: &Path,
    ready: impl FnMut(&[serde_json::Value]) -> bool,
) -> Vec<serde_json::Value> {
    wait_for_events_within(path, Duration::from_secs(10), ready)
}

fn wait_for_first_request(path: &Path, child: &mut process::Child) -> Vec<serde_json::Value> {
    wait_for_child_events(path, child, |events| {
        !request_started_payloads(events).is_empty()
    })
}

/// Wait for the first provider request of a gated run. This spans cold start,
/// so synchronize on the event or child exit rather than a wall-clock budget.
fn wait_for_first_batch_request(path: &Path, child: &mut process::Child) -> Vec<serde_json::Value> {
    wait_for_child_events(path, child, |events| {
        batch_request_started_count(events) >= 1
    })
}

fn wait_for_child_events(
    path: &Path,
    child: &mut process::Child,
    mut ready: impl FnMut(&[serde_json::Value]) -> bool,
) -> Vec<serde_json::Value> {
    loop {
        if path.exists() {
            let events = read_jsonl_lenient(path);
            if ready(&events) {
                return events;
            }
        }
        if let Some(status) = child.try_wait().expect("child exit status should poll") {
            panic!(
                "translate child exited with {status} before expected events appeared in {}",
                path.display()
            );
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn wait_for_child_job_status(
    temp: &TempDir,
    job_id: &str,
    expected: &str,
    child: &mut process::Child,
) {
    // Status is the readiness signal. A wall-clock deadline would turn a
    // runnable-but-unscheduled worker into a false failure, so only premature
    // child exit terminates the wait.
    let db = temp.path().join(".bookforge/jobs.sqlite");
    loop {
        let mut actual = None;
        if db.exists()
            && let Ok(store) = JobStore::open(&db)
            && let Ok(Some(job)) = store.get_job(job_id)
        {
            if job.status == expected {
                return;
            }
            actual = Some(job.status);
        }
        if let Some(status) = child.try_wait().expect("child exit status should poll") {
            panic!(
                "translate child exited with {status} before job {job_id} reached status \
                 {expected}; actual={actual:?}"
            );
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn wait_for_child_corrected_block(temp: &TempDir, job_id: &str, child: &mut process::Child) {
    loop {
        if stored_block_texts(temp, job_id)
            .iter()
            .any(|text| text.contains("[corrected]"))
        {
            return;
        }
        if let Some(status) = child.try_wait().expect("child exit status should poll") {
            panic!(
                "translate child exited with {status} before corrected block was stored for job \
                 {job_id}"
            );
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn wait_for_events_within(
    path: &Path,
    timeout: Duration,
    mut ready: impl FnMut(&[serde_json::Value]) -> bool,
) -> Vec<serde_json::Value> {
    let deadline = Instant::now() + timeout;
    loop {
        if path.exists() {
            let events = read_jsonl_lenient(path);
            if ready(&events) {
                return events;
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for events in {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(25));
    }
}

fn wait_for_job_status(temp: &TempDir, job_id: &str, expected: &str) {
    let db = temp.path().join(".bookforge/jobs.sqlite");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut actual = None;
        if db.exists()
            && let Ok(store) = JobStore::open(&db)
            && let Ok(Some(job)) = store.get_job(job_id)
        {
            if job.status == expected {
                return;
            }
            actual = Some(job.status);
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for job {job_id} status {expected}; actual={actual:?}"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

/// Wait for `child` to exit, returning `None` if it outlives `timeout`.
///
/// On timeout the child is killed and reaped: a test asserting that a process
/// exits must not leave that process running when the assertion fails, and a
/// bounded wait keeps a regression from hanging the suite instead of failing it.
fn wait_for_child_exit(
    child: &mut process::Child,
    timeout: Duration,
) -> Option<process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait().expect("child exit status should poll") {
            Some(status) => return Some(status),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn job_status(temp: &TempDir, job_id: &str) -> String {
    let db = temp.path().join(".bookforge/jobs.sqlite");
    let store = JobStore::open(&db).expect("store should open");
    store
        .get_job(job_id)
        .expect("job should load")
        .expect("job should exist")
        .status
}

fn stored_block_texts(temp: &TempDir, job_id: &str) -> Vec<String> {
    let db = temp.path().join(".bookforge/jobs.sqlite");
    let store = JobStore::open(&db).expect("store should open");
    store
        .load_block_translations(job_id)
        .expect("block translations should load")
        .into_iter()
        .map(|block| block.text)
        .collect()
}

fn event_count(events: &[serde_json::Value], key: &str) -> usize {
    events
        .iter()
        .filter(|event| event.get(key).is_some())
        .count()
}

fn batch_request_started_count(events: &[serde_json::Value]) -> usize {
    events
        .iter()
        .filter(|event| {
            event
                .get("RequestStarted")
                .and_then(|payload| payload.get("batch_id"))
                .and_then(|value| value.as_str())
                .is_some()
        })
        .count()
}

fn batch_request_finished_count(events: &[serde_json::Value]) -> usize {
    events
        .iter()
        .filter(|event| {
            event
                .get("RequestFinished")
                .and_then(|payload| payload.get("batch_id"))
                .and_then(|value| value.as_str())
                .is_some()
        })
        .count()
}

fn finalize_request_started_count(events: &[serde_json::Value]) -> usize {
    finalize_request_started_ids(events).len()
}

fn finalize_request_started_ids(events: &[serde_json::Value]) -> Vec<String> {
    request_started_ids(events)
        .into_iter()
        .filter(|id| is_finalize_request_id(id))
        .collect()
}

fn request_started_ids(events: &[serde_json::Value]) -> Vec<String> {
    events.iter().filter_map(request_started_id).collect()
}

fn request_started_payloads(events: &[serde_json::Value]) -> Vec<&serde_json::Value> {
    events
        .iter()
        .filter_map(|event| event.get("RequestStarted"))
        .collect()
}

fn finalize_request_started_between_pause_and_resume(events: &[serde_json::Value]) -> Vec<String> {
    let mut paused = false;
    let mut ids = Vec::new();
    for event in events {
        if event.get("JobPaused").is_some() {
            paused = true;
            continue;
        }
        if paused && event.get("JobResumed").is_some() {
            break;
        }
        if paused
            && let Some(id) = request_started_id(event)
            && is_finalize_request_id(&id)
        {
            ids.push(id);
        }
    }
    ids
}

fn request_started_id(event: &serde_json::Value) -> Option<String> {
    event
        .get("RequestStarted")
        .and_then(|payload| payload.get("request_id"))
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned)
}

fn request_finished_ids(events: &[serde_json::Value]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| {
            event
                .get("RequestFinished")
                .and_then(|payload| payload.get("request_id"))
                .and_then(|value| value.as_str())
                .map(ToOwned::to_owned)
        })
        .collect()
}

fn is_finalize_request_id(id: &str) -> bool {
    id.starts_with("qa_")
        || id.starts_with("repair_")
        || id.starts_with("double_check_")
        || id.starts_with("fallback_")
}

fn segment_finished_ids(events: &[serde_json::Value]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| event.get("SegmentFinished"))
        .filter_map(|payload| payload.get("segment_id"))
        .filter_map(|value| value.as_str())
        .map(ToOwned::to_owned)
        .collect()
}

fn assert_no_duplicate_segments(ids: &[String]) {
    let unique = ids.iter().collect::<std::collections::HashSet<_>>();
    assert_eq!(
        unique.len(),
        ids.len(),
        "duplicate segment completion: {ids:?}"
    );
}

fn read_jsonl_lenient(path: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn read_jsonl(path: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .expect("JSONL file should exist")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("line should be valid JSON"))
        .collect()
}
