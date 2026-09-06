//! Regression guard for empty-element adjacency (roundtrip identity case).
//!
//! A self-closing sibling (`<p/>`, `<td/>`) adjacent to a real block must
//! never detach that block's translation: the reader folds such a row into
//! one block whose runs mix paired (`<m1>`) and empty (`<r2/>`) markers,
//! and the writer must re-scan those ids from ONE shared ordinal stream or
//! the legitimate marked translation is rejected as "unknown inline marker"
//! and the block silently ships untranslated.

use std::{
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
};

use bookforge_core::{config::BilingualMode, ir::BlockKind, segment::BlockTranslation};
use bookforge_epub::{RebuildOptions, read_epub, rebuild_epub_with_options};
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

const CONTAINER_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;

fn chapter(body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<head><title>Adjacency Fixture</title></head>
<body>
{body}
</body>
</html>"#
    )
}

fn build_epub(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("in.epub");
    let file = File::create(&path).expect("fixture EPUB should be creatable");
    let mut zip = ZipWriter::new(file);
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    zip.start_file("mimetype", stored).unwrap();
    zip.write_all(b"application/epub+zip").unwrap();
    zip.start_file("META-INF/container.xml", deflated).unwrap();
    zip.write_all(CONTAINER_XML.as_bytes()).unwrap();
    let opf = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="uid">adjacency-fixture</dc:identifier>
    <dc:title>Adjacency Fixture</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="ch0" href="ch1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="ch0"/>
  </spine>
</package>"#;
    zip.start_file("content.opf", deflated).unwrap();
    zip.write_all(opf.as_bytes()).unwrap();
    zip.start_file("ch1.xhtml", deflated).unwrap();
    zip.write_all(chapter(body).as_bytes()).unwrap();
    zip.finish().unwrap();
    path
}

fn entry_text(epub: &Path, name: &str) -> String {
    let mut archive = zip::ZipArchive::new(File::open(epub).expect("output should open"))
        .expect("output zip should parse");
    let mut text = String::new();
    Read::read_to_string(
        &mut archive.by_name(name).expect("entry should exist"),
        &mut text,
    )
    .expect("entry should be UTF-8");
    text
}

fn body_region(xhtml: &str) -> &str {
    xhtml
        .split_once("<body>")
        .and_then(|(_, rest)| rest.split_once("</body>"))
        .map(|(body, _)| body)
        .expect("chapter should contain a body element")
}

/// The reader must represent "cell" via a block whose inline payload mixes
/// paired and empty markers exactly as produced by the adjacency shape:
/// this pins the source-side protocol the rebuild depends on.
#[test]
fn reads_empty_siblings_into_mixed_paired_and_empty_inline_markers() {
    let temp = tempfile_dir("read");
    let body = "<p>Real content.</p>\n<p/>\n<p></p>\n<table><tr><td>cell</td><td/></tr></table>";
    let input = build_epub(&temp, body);

    let book = read_epub(&input).expect("EPUB should parse");
    let row_block = book
        .blocks
        .iter()
        .find(|block| block.kind == BlockKind::TableRow)
        .expect("adjacent real cell content should surface in a row-anchored block");

    let marked: String = row_block
        .text_runs
        .iter()
        .map(|run| run.text.as_str())
        .collect();
    assert_eq!(marked, "<m1>cell</m1><r2/>", "reader marker payload");
    assert!(
        row_block
            .inline_marks
            .iter()
            .any(|mark| mark.id == "r2" && mark.kind == "td"),
        "the trailing empty <td/> must consume the shared ordinal stream, got {marked}"
    );
}

/// Identity roundtrip invariant at the epub-crate level: both non-empty
/// blocks receive their translations, empty elements pass through
/// untouched, and nothing is skipped.
#[test]
fn rebuild_translates_nonempty_blocks_after_empty_element_siblings() {
    let temp = tempfile_dir("rebuild");
    let body = "<p>Real content.</p>\n<p/>\n<p></p>\n<table><tr><td>cell</td><td/></tr></table>";
    let input = build_epub(&temp, body);

    let book = read_epub(&input).expect("EPUB should parse");
    let translations = book
        .blocks
        .iter()
        .map(|block| {
            let marked: String = block
                .text_runs
                .iter()
                .map(|run| run.text.as_str())
                .collect();
            BlockTranslation {
                block_id: block.id.clone(),
                text: format!("[Italian] {marked}"),
            }
        })
        .collect::<Vec<_>>();
    let output = temp.join("out.epub");

    let options = RebuildOptions {
        mode: BilingualMode::Replace,
        ..RebuildOptions::replace_with_target_language(Some("Italian"))
    };
    rebuild_epub_with_options(&book, &translations, &output, &options)
        .expect("rebuild should succeed");

    let rebuilt = entry_text(&output, "ch1.xhtml");
    let body = body_region(&rebuilt);

    // Exactly the two non-empty blocks are translated; no prefix before
    // anything else means empty elements stayed out of the payload, while
    // a missing prefix would mean the following block detached.
    assert_eq!(body.matches("[Italian] ").count(), 2, "got: {body}");
    assert!(body.contains("[Italian] Real content."), "got: {body}");
    assert!(
        body.contains("<tr>[Italian] <td>cell</td><td/></tr>"),
        "row block must splice translation and empty siblings back, got: {body}"
    );
}

fn tempfile_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bf-adjacency-{label}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should advance")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir should create");
    dir
}
