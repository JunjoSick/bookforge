use std::{
    fs,
    path::{Path, PathBuf},
};

use assert_cmd::Command;
use bookforge_core::{GlossaryCategory, GlossaryStatus, GlossaryTerm};
use bookforge_store::{GlossaryFilter, JobStore, NewGlossaryCandidate};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

fn bookforge() -> Command {
    Command::cargo_bin("bookforge").expect("bookforge binary should be built")
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("cli crate should be under crates/bookforge-cli")
        .to_path_buf()
}

fn fixture_input() -> PathBuf {
    workspace_root().join("test/test.epub")
}

#[test]
fn cli_translate_mock_quiet_writes_output_report_and_events() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let run = translate_quiet(&temp, "mock-prefix-target");

    assert!(run.output.exists(), "translated EPUB should exist");
    assert!(run.events.exists(), "event log should exist");
    assert!(run.report.exists(), "markdown report should exist");
}

#[test]
fn cli_translate_context_window_persists_snapshot_settings() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let output = temp.path().join("out.epub");
    let events = temp.path().join("events.jsonl");
    bookforge()
        .current_dir(temp.path())
        .args([
            "translate",
            fixture_input().to_str().unwrap(),
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
fn cli_translate_with_style_sheet_persists_rendered_block_in_snapshot() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
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
            fixture_input().to_str().unwrap(),
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
            fixture_input().to_str().unwrap(),
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
    let output = temp.path().join("out.epub");
    let events = temp.path().join("events.jsonl");
    bookforge()
        .current_dir(temp.path())
        .args([
            "translate",
            fixture_input().to_str().unwrap(),
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
            fixture_input().to_str().unwrap(),
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

    bookforge()
        .current_dir(temp.path())
        .args([
            "glossary",
            "extract-candidates",
            fixture_input().to_str().unwrap(),
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
        candidates
            .iter()
            .any(|candidate| candidate.source_text == "Ivan Ilych")
    );
    assert!(candidates.iter().all(|candidate| {
        candidate.target_text.is_none() && candidate.status == GlossaryStatus::AutoCandidate
    }));
}

#[test]
fn cli_glossary_review_candidates_accepts_sets_and_rejects() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
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
            fixture_input().to_str().unwrap(),
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
    let output = temp.path().join("json.epub");
    let events = temp.path().join("json-events.jsonl");
    let assert = bookforge()
        .current_dir(temp.path())
        .args([
            "translate",
            fixture_input().to_str().unwrap(),
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
fn cli_resume_uses_input_snapshot_after_original_is_moved() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let input = temp.path().join("source.epub");
    let moved = temp.path().join("source-moved.epub");
    fs::copy(fixture_input(), &input).expect("fixture should copy");
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
    translate_quiet_input(temp, &fixture_input(), model)
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

fn read_jsonl(path: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .expect("JSONL file should exist")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("line should be valid JSON"))
        .collect()
}
