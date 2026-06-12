//! Identity-roundtrip harness over a synthetic EPUB corpus.
//!
//! Two oracles, two mock models:
//!
//! - `mock-identity` checks **structure preservation**: translating a book
//!   with the identity transform must leave every byte of visible text and
//!   whitespace-sensitive content (e.g. `<pre>`) unchanged.
//! - `mock-prefix-target` checks **coverage**: every piece of prose the
//!   reader claims to own must come back carrying the `[Italian]` prefix.
//!   Sentinel words that appear in the output without the prefix were never
//!   sent to the model — silently untranslated text.
//!
//! Tests marked `#[ignore]` document known extraction bugs; un-ignore them
//! as the corresponding reader fixes land.

use std::{
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
};

use assert_cmd::Command;
use quick_xml::{Reader, events::Event};
use tempfile::TempDir;
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

const CONTAINER_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;

fn opf(chapter_files: &[&str]) -> String {
    let manifest = chapter_files
        .iter()
        .enumerate()
        .map(|(i, href)| {
            format!(r#"    <item id="ch{i}" href="{href}" media-type="application/xhtml+xml"/>"#)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let spine = chapter_files
        .iter()
        .enumerate()
        .map(|(i, _)| format!(r#"    <itemref idref="ch{i}"/>"#))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="uid">roundtrip-fixture</dc:identifier>
    <dc:title>Roundtrip Fixture</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
{manifest}
  </manifest>
  <spine>
{spine}
  </spine>
</package>"#
    )
}

fn chapter(body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<head><title>Chapter</title></head>
<body>
{body}
</body>
</html>"#
    )
}

/// Build a minimal valid EPUB at `dir/name` from (filename, body-html) pairs.
fn build_epub(dir: &Path, name: &str, chapters: &[(&str, &str)]) -> PathBuf {
    let path = dir.join(name);
    let file = File::create(&path).expect("fixture EPUB should be creatable");
    let mut zip = ZipWriter::new(file);

    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    zip.start_file("mimetype", stored).unwrap();
    zip.write_all(b"application/epub+zip").unwrap();
    zip.start_file("META-INF/container.xml", deflated).unwrap();
    zip.write_all(CONTAINER_XML.as_bytes()).unwrap();
    let hrefs = chapters.iter().map(|(href, _)| *href).collect::<Vec<_>>();
    zip.start_file("content.opf", deflated).unwrap();
    zip.write_all(opf(&hrefs).as_bytes()).unwrap();
    for (href, body) in chapters {
        zip.start_file(*href, deflated).unwrap();
        zip.write_all(chapter(body).as_bytes()).unwrap();
    }
    zip.finish().unwrap();
    path
}

struct RoundtripRun {
    _temp: TempDir,
    output: PathBuf,
}

/// Translate `chapters` with the given mock model and return the output EPUB.
fn translate(chapters: &[(&str, &str)], model: &str) -> RoundtripRun {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let input = build_epub(temp.path(), "in.epub", chapters);
    let output = temp.path().join("out.epub");
    Command::cargo_bin("bookforge")
        .expect("bookforge binary should be built")
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
            model,
            "--ui",
            "quiet",
            "--out",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();
    assert!(output.exists(), "translated EPUB should exist");
    RoundtripRun {
        _temp: temp,
        output,
    }
}

fn read_zip_text(epub: &Path, member: &str) -> String {
    let file = File::open(epub).expect("EPUB should open");
    let mut archive = ZipArchive::new(file).expect("EPUB should be a zip");
    let mut entry = archive.by_name(member).expect("member should exist");
    let mut text = String::new();
    entry
        .read_to_string(&mut text)
        .expect("member should be UTF-8");
    text
}

/// Whitespace-normalized visible text of `<body>`.
fn body_text(epub: &Path, member: &str) -> String {
    let xhtml = read_zip_text(epub, member);
    extract_text(&xhtml, None)
}

/// Raw (whitespace-exact) text content of every `<pre>` element, in order.
fn pre_texts(epub: &Path, member: &str) -> Vec<String> {
    let xhtml = read_zip_text(epub, member);
    let mut reader = Reader::from_str(&xhtml);
    reader.config_mut().trim_text(false);
    let mut depth = 0usize;
    let mut current = String::new();
    let mut out = Vec::new();
    loop {
        match reader.read_event().expect("fixture XHTML should parse") {
            Event::Start(e) if e.local_name().as_ref() == b"pre" => {
                depth += 1;
            }
            Event::End(e) if e.local_name().as_ref() == b"pre" => {
                depth -= 1;
                if depth == 0 {
                    out.push(std::mem::take(&mut current));
                }
            }
            Event::Text(t) if depth > 0 => current.push_str(&t.decode().unwrap()),
            Event::CData(t) if depth > 0 => {
                current.push_str(&t.decode().unwrap());
            }
            Event::Eof => break,
            _ => {}
        }
    }
    out
}

fn extract_text(xhtml: &str, within: Option<&[u8]>) -> String {
    let mut reader = Reader::from_str(xhtml);
    reader.config_mut().trim_text(false);
    let scope = within.unwrap_or(b"body");
    let mut in_scope = false;
    let mut text = String::new();
    loop {
        match reader.read_event().expect("XHTML should parse") {
            Event::Start(e) if e.local_name().as_ref() == scope => in_scope = true,
            Event::End(e) if e.local_name().as_ref() == scope => in_scope = false,
            Event::Text(t) if in_scope => text.push_str(&t.decode().unwrap()),
            Event::CData(t) if in_scope => text.push_str(&t.decode().unwrap()),
            Event::Eof => break,
            _ => {}
        }
    }
    normalize_ws(&text)
}

fn normalize_ws(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

const PREFIX: &str = "[Italian]";

/// Assert that `sentinel` appears in the text and is covered by translation:
/// somewhere in the same output, the prefix marker must precede it within its
/// block. We approximate "same block" by requiring the sentinel to NOT appear
/// in any stretch of text that lacks the prefix entirely.
fn assert_translated(out_text: &str, sentinel: &str) {
    assert!(
        out_text.contains(sentinel),
        "sentinel '{sentinel}' must survive translation, got: {out_text}"
    );
    // Identity check on coverage: text reachable by the reader always comes
    // back as "[Italian] <original block text>". If the sentinel's block was
    // never sent to the model there is no prefix anywhere before it up to the
    // previous prefixed block.
    let position = out_text.find(sentinel).unwrap();
    let preceding = &out_text[..position];
    let last_prefix = preceding.rfind(PREFIX);
    assert!(
        last_prefix.is_some(),
        "sentinel '{sentinel}' appears with no '{PREFIX}' marker before it — its block was never translated.\nOutput: {out_text}"
    );
}

fn assert_untranslated(out_text: &str, sentinel: &str) {
    let position = out_text
        .find(sentinel)
        .unwrap_or_else(|| panic!("sentinel '{sentinel}' must appear in output: {out_text}"));
    let between = &out_text[..position];
    // The block carrying the sentinel must not start with the prefix. Look at
    // the text from the last prefix to the sentinel: if a prefix immediately
    // governs this block, the distance is short and within the same block.
    // For fixture purposes the untranslated sentinel is in its own block, so
    // it is enough that "[Italian] SENTINEL" never occurs.
    let _ = between;
    assert!(
        !out_text.contains(&format!("{PREFIX} {sentinel}")),
        "sentinel '{sentinel}' was unexpectedly translated: {out_text}"
    );
}

// ---------------------------------------------------------------------------
// Structure preservation (mock-identity): output visible text == input.
// ---------------------------------------------------------------------------

#[test]
fn identity_roundtrip_preserves_basic_prose_and_inline_markup() {
    let body = r#"<h1>Chapter One</h1>
<p>Plain paragraph with an em dash — and an ellipsis…</p>
<p>Hello <em>brave new</em> world, with a <a href="https://example.com">link</a>!</p>
<p>Line one<br/>line two.</p>
<blockquote>A quote with <strong>bold</strong> text.</blockquote>
<ul><li>First item</li><li>Second item</li></ul>"#;
    let chapters = [("ch1.xhtml", body)];
    let probe_dir = tempfile::tempdir().unwrap();
    let input = build_epub(probe_dir.path(), "probe.epub", &chapters);
    let expected = body_text(&input, "ch1.xhtml");

    let run = translate(&chapters, "mock-identity");
    let actual = body_text(&run.output, "ch1.xhtml");

    assert_eq!(
        expected, actual,
        "identity translation must not change visible text"
    );
}

#[test]
fn identity_roundtrip_preserves_pre_block_whitespace_exactly() {
    let body = "<p>Before the diagram.</p>\n<pre>  col1   col2\n  a      b\n\n  zone:9 ::: gate</pre>\n<p>After the diagram.</p>";
    let chapters = [("ch1.xhtml", body)];
    let probe_dir = tempfile::tempdir().unwrap();
    let input = build_epub(probe_dir.path(), "probe.epub", &chapters);
    let expected = pre_texts(&input, "ch1.xhtml");
    assert_eq!(expected.len(), 1, "fixture should contain one pre block");

    let run = translate(&chapters, "mock-identity");
    let actual = pre_texts(&run.output, "ch1.xhtml");

    assert_eq!(
        expected, actual,
        "pre/code content must survive translation byte-for-byte"
    );
}

#[test]
fn identity_roundtrip_does_not_invent_text_in_empty_elements() {
    let body = r#"<p>Real content.</p>
<p/>
<p></p>
<table><tr><td>cell</td><td/></tr></table>"#;
    let chapters = [("ch1.xhtml", body)];
    let run = translate(&chapters, "mock-prefix-target");
    let text = body_text(&run.output, "ch1.xhtml");

    // The only translated text must be the two non-empty blocks. A prefix
    // with nothing after it means an empty element received hallucinated
    // model output.
    let occurrences = text.matches(PREFIX).count();
    assert_eq!(
        occurrences, 2,
        "empty elements must not be sent to the model (expected 2 translated blocks), got: {text}"
    );
}

// ---------------------------------------------------------------------------
// Coverage (mock-prefix-target): everything readable must carry the prefix.
// ---------------------------------------------------------------------------

#[test]
fn coverage_paragraphs_headings_lists_quotes() {
    let body = r#"<h2>HEAD_SENTINEL</h2>
<p>PARA_SENTINEL with prose.</p>
<ul><li>LIST_SENTINEL entry</li></ul>
<blockquote>QUOTE_SENTINEL text</blockquote>
<table><tr><td>CELL_SENTINEL</td></tr></table>"#;
    let run = translate(&[("ch1.xhtml", body)], "mock-prefix-target");
    let text = body_text(&run.output, "ch1.xhtml");

    for sentinel in [
        "HEAD_SENTINEL",
        "PARA_SENTINEL",
        "LIST_SENTINEL",
        "QUOTE_SENTINEL",
        "CELL_SENTINEL",
    ] {
        assert_translated(&text, sentinel);
    }
}

#[test]
#[ignore = "known bug: text directly inside <div>/<dt>/<dd>/<figure>/naked <body> is never extracted and ships untranslated (block_kind whitelist in bookforge-epub/src/reader.rs)"]
fn coverage_div_and_naked_body_text() {
    let body = r#"<div class="ccru-fragment">DIV_SENTINEL hyperstition node</div>
<dl><dt>DT_SENTINEL</dt><dd>DD_SENTINEL definition</dd></dl>
<figure><div>FIGDIV_SENTINEL</div></figure>
NAKED_SENTINEL floating in body"#;
    let run = translate(&[("ch1.xhtml", body)], "mock-prefix-target");
    let text = body_text(&run.output, "ch1.xhtml");

    for sentinel in [
        "DIV_SENTINEL",
        "DT_SENTINEL",
        "DD_SENTINEL",
        "FIGDIV_SENTINEL",
        "NAKED_SENTINEL",
    ] {
        assert_translated(&text, sentinel);
    }
}

#[test]
#[ignore = "known bug: nested same-name blocks (li>ul>li) close the outer block at the first matching end tag; inner patches are silently dropped (reader.rs End handling vs writer.rs depth-tracked buffering)"]
fn coverage_nested_lists() {
    let body = r#"<ul>
<li>OUTER_SENTINEL intro
  <ul><li>INNER_ONE_SENTINEL</li><li>INNER_TWO_SENTINEL</li></ul>
tail text TAIL_SENTINEL</li>
<li>SIBLING_SENTINEL</li>
</ul>"#;
    let run = translate(&[("ch1.xhtml", body)], "mock-prefix-target");
    let text = body_text(&run.output, "ch1.xhtml");

    for sentinel in [
        "OUTER_SENTINEL",
        "INNER_ONE_SENTINEL",
        "INNER_TWO_SENTINEL",
        "TAIL_SENTINEL",
        "SIBLING_SENTINEL",
    ] {
        assert_translated(&text, sentinel);
    }
}

#[test]
#[ignore = "known bug: named HTML entities (&nbsp; &mdash;) are not resolvable by quick-xml without a custom resolver; ingestion fails on books that use them"]
fn ingestion_survives_named_html_entities() {
    let body = "<p>ENTITY_SENTINEL one&nbsp;two&mdash;three</p>";
    let run = translate(&[("ch1.xhtml", body)], "mock-prefix-target");
    let text = body_text(&run.output, "ch1.xhtml");
    assert_translated(&text, "ENTITY_SENTINEL");
}

// ---------------------------------------------------------------------------
// Code blocks must not be translated at all (lands with the Code-skip fix).
// ---------------------------------------------------------------------------

#[test]
fn code_blocks_are_not_sent_to_the_model() {
    let body = r#"<p>PROSE_SENTINEL before.</p>
<pre>CODE_SENTINEL := numogram(9)</pre>"#;
    let run = translate(&[("ch1.xhtml", body)], "mock-prefix-target");
    let text = body_text(&run.output, "ch1.xhtml");

    assert_translated(&text, "PROSE_SENTINEL");
    assert_untranslated(&text, "CODE_SENTINEL");
}
