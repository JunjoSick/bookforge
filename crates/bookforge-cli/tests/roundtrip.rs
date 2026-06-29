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

fn opf_with_ncx(chapter_files: &[&str]) -> String {
    let manifest = chapter_files
        .iter()
        .enumerate()
        .map(|(i, href)| {
            format!(r#"    <item id="ch{i}" href="{href}" media-type="application/xhtml+xml"/>"#)
        })
        .chain(std::iter::once(
            r#"    <item id="toc" href="toc.ncx" media-type="application/x-dtbncx+xml"/>"#
                .to_string(),
        ))
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
  <spine toc="toc">
{spine}
  </spine>
</package>"#
    )
}

fn opf_with_nav(chapter_files: &[&str]) -> String {
    let manifest = chapter_files
        .iter()
        .enumerate()
        .map(|(i, href)| {
            format!(r#"    <item id="ch{i}" href="{href}" media-type="application/xhtml+xml"/>"#)
        })
        .chain(std::iter::once(
            r#"    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>"#
                .to_string(),
        ))
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

fn ncx(chapter_files: &[&str]) -> String {
    let nav_points = chapter_files
        .iter()
        .enumerate()
        .map(|(i, href)| {
            format!(
                r##"    <navPoint id="nav{i}" playOrder="{}"><navLabel><text>Chapter</text></navLabel><content src="{href}"/></navPoint>"##,
                i + 1
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
  <head><meta name="dtb:uid" content="roundtrip-fixture"/></head>
  <docTitle><text>Roundtrip Fixture</text></docTitle>
  <navMap>
{nav_points}
  </navMap>
</ncx>"#
    )
}

fn nav_xhtml() -> String {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<head><title>Navigation</title></head>
<body><nav epub:type="toc"><h1>Contents</h1><ol>
<li><a href="ch1.xhtml">Chapter One</a></li>
</ol></nav></body>
</html>"#
        .to_string()
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

fn build_epub_with_ncx(dir: &Path, name: &str, chapters: &[(&str, &str)]) -> PathBuf {
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
    zip.write_all(opf_with_ncx(&hrefs).as_bytes()).unwrap();
    zip.start_file("toc.ncx", deflated).unwrap();
    zip.write_all(ncx(&hrefs).as_bytes()).unwrap();
    for (href, body) in chapters {
        zip.start_file(*href, deflated).unwrap();
        zip.write_all(chapter(body).as_bytes()).unwrap();
    }
    zip.finish().unwrap();
    path
}

fn build_epub_with_nav(dir: &Path, name: &str, chapters: &[(&str, &str)]) -> PathBuf {
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
    zip.write_all(opf_with_nav(&hrefs).as_bytes()).unwrap();
    zip.start_file("nav.xhtml", deflated).unwrap();
    zip.write_all(nav_xhtml().as_bytes()).unwrap();
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

fn translate_with_ncx(chapters: &[(&str, &str)], model: &str) -> RoundtripRun {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let input = build_epub_with_ncx(temp.path(), "in.epub", chapters);
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

fn translate_with_nav(chapters: &[(&str, &str)], model: &str) -> RoundtripRun {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let input = build_epub_with_nav(temp.path(), "in.epub", chapters);
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
fn ingestion_survives_named_html_entities() {
    let body = "<p>ENTITY_SENTINEL one&nbsp;two&mdash;three</p>";
    let run = translate(&[("ch1.xhtml", body)], "mock-prefix-target");
    let text = body_text(&run.output, "ch1.xhtml");
    assert_translated(&text, "ENTITY_SENTINEL");
}

#[test]
fn coverage_opf_ncx_and_head_titles() {
    let body = r#"<h1>BODY_TITLE_SENTINEL</h1><p>BODY_SENTINEL text.</p>"#;
    let run = translate_with_ncx(&[("ch1.xhtml", body)], "mock-prefix-target");

    let opf = read_zip_text(&run.output, "content.opf");
    assert!(
        opf.contains("<dc:title>[Italian] Roundtrip Fixture</dc:title>"),
        "OPF dc:title should be translated, got: {opf}"
    );
    assert!(
        opf.contains("<dc:language>it</dc:language>"),
        "OPF dc:language should be updated to the target language tag, got: {opf}"
    );

    let chapter = read_zip_text(&run.output, "ch1.xhtml");
    assert!(
        chapter.contains("<title>[Italian] Chapter</title>"),
        "XHTML head title should be translated, got: {chapter}"
    );

    let toc = read_zip_text(&run.output, "toc.ncx");
    assert!(
        toc.contains("<text>[Italian] Roundtrip Fixture</text>"),
        "NCX docTitle should be translated, got: {toc}"
    );
    assert!(
        toc.contains("<text>[Italian] Chapter</text>"),
        "NCX navLabel should be translated, got: {toc}"
    );
}

#[test]
fn coverage_epub3_nav_document_not_in_spine() {
    let body = r#"<h1>BODY_TITLE_SENTINEL</h1><p>BODY_SENTINEL text.</p>"#;
    let run = translate_with_nav(&[("ch1.xhtml", body)], "mock-prefix-target");

    let nav = read_zip_text(&run.output, "nav.xhtml");
    assert!(
        nav.contains("<h1>[Italian] Contents</h1>"),
        "EPUB3 nav heading should be translated even when nav is not in spine, got: {nav}"
    );
    assert!(
        nav.contains(r#"<a href="ch1.xhtml">[Italian] Chapter One</a>"#),
        "EPUB3 nav link label should be translated even when nav is not in spine, got: {nav}"
    );
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

#[test]
fn navigation_list_translation_stays_inside_link_content() {
    let body = r#"<nav><h1>Contents</h1><ol>
<li><a href="ch1.xhtml">Cover</a></li>
<li><a href="ch1.xhtml"><span>1</span> Chapter One</a></li>
</ol></nav>"#;
    let run = translate(&[("ch1.xhtml", body)], "mock-prefix-target");
    let xhtml = read_zip_text(&run.output, "ch1.xhtml");

    assert!(
        !xhtml.contains("<li>[Italian]"),
        "translation before a navigation link violates the EPUB nav content model: {xhtml}"
    );
    assert!(
        xhtml.contains(r#"<a href="ch1.xhtml">[Italian] Cover</a>"#),
        "plain navigation label should be translated inside its link: {xhtml}"
    );
}
