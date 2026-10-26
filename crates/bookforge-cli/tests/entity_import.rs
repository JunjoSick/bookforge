//! Global entity sheets share one scope per language pair, and their notes
//! reach the translation guidance saved with a job.

use std::{fs, io::Write, path::Path, time::Duration};

use assert_cmd::Command;
use bookforge_core::GlossaryScopeKind;
use bookforge_store::JobStore;
use serde_json::Value;
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

fn bookforge(dir: &Path) -> Command {
    let mut command = Command::cargo_bin("bookforge").expect("bookforge binary should be built");
    command.current_dir(dir).timeout(Duration::from_secs(60));
    command
}

/// A global sheet for one entity. `scope_id` reproduces the stray id older
/// imports stored for global sheets.
fn write_sheet(path: &Path, target_name: &str, notes: &str, scope_id: Option<&str>) {
    let id_line = scope_id
        .map(|id| format!("id = \"{id}\"\n"))
        .unwrap_or_default();
    fs::write(
        path,
        format!(
            "[meta]\nschema_version = 1\nsource_language = \"en\"\ntarget_language = \"it\"\n\n\
             [meta.scope]\nkind = \"global\"\n{id_line}\n\
             [[entity]]\nsource_name = \"Quillon\"\ntarget_name = \"{target_name}\"\n\
             gender_target = \"f\"\nnotes = \"{notes}\"\n"
        ),
    )
    .expect("entity sheet should be writable");
}

fn write_epub(path: &Path) {
    let entries = [
        ("mimetype", "application/epub+zip"),
        (
            "META-INF/container.xml",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#,
        ),
        (
            "content.opf",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="uid">entity-import-fixture</dc:identifier>
    <dc:title>Entity Import Fixture</dc:title>
    <dc:language>en</dc:language>
    <meta property="dcterms:modified">2026-01-01T00:00:00Z</meta>
  </metadata>
  <manifest>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
    <item id="chapter" href="chapter.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine><itemref idref="chapter"/></spine>
</package>"#,
        ),
        (
            "nav.xhtml",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<head><title>Contents</title></head>
<body><nav epub:type="toc"><ol><li><a href="chapter.xhtml">Harbor</a></li></ol></nav></body>
</html>"#,
        ),
        (
            "chapter.xhtml",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<head><title>Harbor</title></head>
<body><h1>Harbor</h1><p>Quillon checked the tide tables before dawn.</p></body>
</html>"#,
        ),
    ];
    let mut archive = ZipWriter::new(fs::File::create(path).expect("EPUB should be creatable"));
    for (name, contents) in entries {
        let method = if name == "mimetype" {
            CompressionMethod::Stored
        } else {
            CompressionMethod::Deflated
        };
        archive
            .start_file(
                name,
                SimpleFileOptions::default().compression_method(method),
            )
            .unwrap();
        archive.write_all(contents.as_bytes()).unwrap();
    }
    archive.finish().unwrap();
}

fn job_id_from_events(path: &Path) -> String {
    fs::read_to_string(path)
        .expect("event log should exist")
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find_map(|event| event["JobCreated"]["job_id"].as_str().map(str::to_owned))
        .expect("event log should include the job id")
}

/// Imports a sheet with a stray global id, then a corrected sheet without
/// one, either with `entities import` or as `translate --entities` files.
fn assert_corrected_global_entity_wins(pass_files_to_translate: bool) {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let stale_sheet = temp.path().join("stale.toml");
    let corrected_sheet = temp.path().join("corrected.toml");
    let source = temp.path().join("source.epub");
    let output = temp.path().join("translated.epub");
    let events = temp.path().join("events.jsonl");
    write_sheet(
        &stale_sheet,
        "Quillona",
        "Stale guidance.",
        Some("leftover-series"),
    );
    write_sheet(
        &corrected_sheet,
        "Chilona",
        "Call her Capitana Chilona.",
        None,
    );
    write_epub(&source);

    if !pass_files_to_translate {
        for sheet in [&stale_sheet, &corrected_sheet] {
            bookforge(temp.path())
                .args(["entities", "import"])
                .arg(sheet)
                .assert()
                .success();
        }
    }
    let mut translate = bookforge(temp.path());
    translate
        .arg("translate")
        .arg(&source)
        .args([
            "--provider",
            "mock",
            "--model",
            "mock-prefix-target",
            "--source",
            "en",
            "--target",
            "it",
            "--qa",
            "off",
            "--ui",
            "quiet",
            "--out",
        ])
        .arg(&output)
        .arg("--progress-jsonl")
        .arg(&events);
    if pass_files_to_translate {
        translate
            .arg("--entities")
            .arg(&stale_sheet)
            .arg("--entities")
            .arg(&corrected_sheet);
    }
    translate.assert().success();

    let job_id = job_id_from_events(&events);
    let store = JobStore::open(temp.path().join(".bookforge/jobs.sqlite")).unwrap();
    let globals = store
        .list_entities(
            Some("en"),
            Some("it"),
            Some(GlossaryScopeKind::Global),
            None,
        )
        .unwrap();
    assert_eq!(globals.len(), 1, "a stray id must not split global scope");
    assert_eq!(globals[0].scope_id, None);
    assert_eq!(globals[0].target_name, "Chilona");

    let snapshot = store.load_job_config_snapshot(&job_id).unwrap().unwrap();
    let block = &snapshot.entities_rendered_block;
    assert!(block.contains("- Chilona (Quillon): feminine\n"), "{block}");
    assert!(
        block.contains("  Notes: Call her Capitana Chilona.\n"),
        "entity notes must reach the saved translation guidance: {block}"
    );
    assert!(!block.contains("Quillona"), "{block}");
    assert!(!block.contains("Stale guidance"), "{block}");

    bookforge(temp.path())
        .args([
            "entities",
            "show",
            "--source-language",
            "en",
            "--target-language",
            "it",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("Chilona (Quillon)"));
}

#[test]
fn entity_import_normalizes_global_scope_before_applying_a_correction() {
    assert_corrected_global_entity_wins(false);
}

#[test]
fn translation_entity_files_share_one_global_scope() {
    assert_corrected_global_entity_wins(true);
}
