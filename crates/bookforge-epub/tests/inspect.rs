use std::{
    fs::File,
    io::{Read, Write},
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use bookforge_core::{
    config::BilingualMode,
    config::SegmentationConfig,
    ir::BlockKind,
    segment::{BlockTranslation, build_segments},
};
use bookforge_epub::{
    RebuildOptions, inspect_epub, read_epub, rebuild_epub, rebuild_epub_with_options,
};
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

#[test]
fn inspects_minimal_epub() {
    let fixture = create_minimal_epub();
    let inspection = inspect_epub(&fixture).expect("fixture should inspect successfully");

    assert_eq!(
        inspection.title.as_deref(),
        Some("Generated Bookforge Fixture")
    );
    assert_eq!(inspection.spine_count, 1);
    assert_eq!(inspection.manifest_count, 2);
    assert_eq!(inspection.xhtml_count, 2);
    assert_eq!(inspection.xhtml_spine_count, 1);
    assert!(inspection.has_nav);
    assert!(!inspection.has_toc);
    assert_eq!(inspection.resource_count, 0);
}

#[test]
fn builds_basic_ir_from_minimal_epub() {
    let fixture = create_minimal_epub();
    let book = read_epub(&fixture).expect("fixture should parse into IR");

    assert_eq!(book.sections.len(), 3);
    assert_eq!(book.blocks.len(), 5);
    assert!(book.blocks.iter().any(|block| {
        block.kind == BlockKind::Paragraph && block_text(block) == "Generated Bookforge Fixture"
    }));
    assert!(book.blocks.iter().any(
        |block| block.kind == BlockKind::Paragraph && block_text(block) == "Generated Fixture"
    ));
    assert!(book.blocks.iter().any(|block| {
        block.kind == BlockKind::Paragraph && block_text(block) == "Hello from chapter 1."
    }));
    assert!(
        book.blocks
            .iter()
            .any(|block| block.kind == BlockKind::Paragraph && block_text(block) == "Navigation")
    );
    assert!(book.blocks.iter().all(|block| block.token_estimate > 0));
}

#[test]
fn builds_stable_segments_from_minimal_epub() {
    let fixture = create_minimal_epub();
    let book = read_epub(&fixture).expect("fixture should parse into IR");
    let config = SegmentationConfig {
        max_segment_tokens: 1_200,
        context_tokens: 8,
    };

    let first = build_segments(&book, &config).expect("segments should build");
    let second = build_segments(&book, &config).expect("segments should be repeatable");

    assert_eq!(first.len(), 3);
    assert_eq!(first[0].id, second[0].id);
    assert_eq!(first[0].checksum, second[0].checksum);
    assert_eq!(first[0].section_id.0, "sec_metadata_opf");
    assert_eq!(first[0].block_ids.len(), 1);
    assert!(first[0].source.text.contains("Generated Bookforge Fixture"));
    assert_eq!(first[1].section_id.0, "sec_000000");
    assert!(first[1].source.text.contains("Generated Fixture"));
    assert!(first[1].source.text.contains("Hello from chapter 1."));
    assert_eq!(first[2].section_id.0, "sec_nav_000000");
    assert!(first[2].source.text.contains("Navigation"));
    assert!(first[2].source.text.contains("Generated Fixture"));
    assert!(first[0].source.token_estimate > 0);
    assert!(first[0].context.before.is_none());
    assert!(first[0].context.after.is_some());
}

#[test]
fn rebuilds_epub_with_patched_xhtml_and_preserved_resources() {
    let fixture = create_minimal_epub();
    let book = read_epub(&fixture).expect("fixture should parse into IR");
    let output =
        std::env::temp_dir().join(format!("bookforge-rebuilt-{}.epub", std::process::id()));
    let _ = std::fs::remove_file(&output);
    let body_block = book
        .blocks
        .iter()
        .find(|block| block_text(block) == "Hello from chapter 1.")
        .expect("body paragraph should be extracted");

    rebuild_epub(
        &book,
        &[BlockTranslation {
            block_id: body_block.id.clone(),
            text: "Ciao da un EPUB minimo.".to_string(),
        }],
        &output,
    )
    .expect("EPUB should rebuild");

    let inspection = inspect_epub(&output).expect("rebuilt EPUB should inspect");
    assert_eq!(inspection.spine_count, 1);
    assert_eq!(inspection.manifest_count, 2);

    let mut archive = ZipArchive::new(File::open(&output).expect("rebuilt EPUB should exist"))
        .expect("rebuilt EPUB should be a zip");
    let mut chapter = String::new();
    archive
        .by_name("OEBPS/chapter1.xhtml")
        .expect("chapter should be present")
        .read_to_string(&mut chapter)
        .expect("chapter should be UTF-8");

    assert!(chapter.contains("Ciao da un EPUB minimo."));
}

#[test]
fn rebuild_can_safely_replace_the_source_path() {
    let fixture = create_minimal_epub();
    let book = read_epub(&fixture).expect("fixture should parse into IR");
    let body_block = book
        .blocks
        .iter()
        .find(|block| block_text(block) == "Hello from chapter 1.")
        .expect("body paragraph should be extracted");

    rebuild_epub(
        &book,
        &[BlockTranslation {
            block_id: body_block.id.clone(),
            text: "Safely replaced in place.".to_string(),
        }],
        &fixture,
    )
    .expect("staged rebuild should replace the source path");

    let mut archive =
        ZipArchive::new(File::open(&fixture).expect("replaced EPUB should exist")).expect("zip");
    let mut chapter = String::new();
    archive
        .by_name("OEBPS/chapter1.xhtml")
        .expect("chapter should remain present")
        .read_to_string(&mut chapter)
        .expect("chapter should read");
    assert!(chapter.contains("Safely replaced in place."));
}

#[test]
fn rebuild_replace_options_are_byte_identical_to_default_rebuild() {
    let fixture = create_minimal_epub();
    let book = read_epub(&fixture).expect("fixture should parse into IR");
    let body_block = book
        .blocks
        .iter()
        .find(|block| block_text(block) == "Hello from chapter 1.")
        .expect("body paragraph should be extracted");
    let translations = [BlockTranslation {
        block_id: body_block.id.clone(),
        text: "Ciao da un EPUB minimo.".to_string(),
    }];
    let output_default = std::env::temp_dir().join(format!(
        "bookforge-replace-default-{}.epub",
        std::process::id()
    ));
    let output_options = std::env::temp_dir().join(format!(
        "bookforge-replace-options-{}.epub",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&output_default);
    let _ = std::fs::remove_file(&output_options);

    rebuild_epub(&book, &translations, &output_default).expect("default rebuild should succeed");
    rebuild_epub_with_options(
        &book,
        &translations,
        &output_options,
        &RebuildOptions {
            mode: BilingualMode::Replace,
            ..RebuildOptions::default()
        },
    )
    .expect("explicit replace rebuild should succeed");

    assert_eq!(
        std::fs::read(&output_default).expect("default output should read"),
        std::fs::read(&output_options).expect("options output should read"),
        "replace mode must remain byte-identical to the default rebuild path"
    );
}

#[test]
fn bilingual_rebuild_keeps_document_language_and_marks_only_translation() {
    let fixture = create_minimal_epub();
    let book = read_epub(&fixture).expect("fixture should parse into IR");
    let body_block = book
        .blocks
        .iter()
        .find(|block| block_text(block) == "Hello from chapter 1.")
        .expect("body paragraph should be extracted");
    let output = std::env::temp_dir().join(format!(
        "bookforge-bilingual-language-{}.epub",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&output);

    rebuild_epub_with_options(
        &book,
        &[BlockTranslation {
            block_id: body_block.id.clone(),
            text: "Ciao dal capitolo uno.".to_string(),
        }],
        &output,
        &RebuildOptions {
            target_language: Some("Italian".to_string()),
            mode: BilingualMode::AppendBlock,
            ..RebuildOptions::default()
        },
    )
    .expect("bilingual rebuild should succeed");

    let mut archive = ZipArchive::new(File::open(&output).unwrap()).unwrap();
    let mut chapter = String::new();
    archive
        .by_name("OEBPS/chapter1.xhtml")
        .unwrap()
        .read_to_string(&mut chapter)
        .unwrap();
    let root = chapter.split_once("<head").unwrap().0;
    assert!(!root.contains("lang=\"it\""));
    assert!(chapter.contains("class=\"bookforge-translation\" lang=\"it\""));
}

#[test]
fn parses_complex_generated_fixture_shapes() {
    let fixture = create_epub_fixture(
        "complex",
        r##"
        <h1>Complex Fixture</h1>
        <p>Hello <em>formatted</em> <a href="https://example.com">link</a> with note <sup><a href="#fn1" epub:type="noteref">1</a></sup>.</p>
        <ul>
          <li>First list item</li>
          <li>Second <strong>list item</strong></li>
        </ul>
        <table>
          <tr><th>Year </th><th>Value</th></tr>
          <tr><td>2024 </td><td>42%</td></tr>
        </table>
        <aside epub:type="footnote" id="fn1">Footnote <em>body</em>.</aside>
        "##,
    );

    let book = read_epub(&fixture).expect("complex fixture should parse");
    let kinds = book
        .blocks
        .iter()
        .map(|block| block.kind)
        .collect::<Vec<_>>();

    assert!(kinds.contains(&BlockKind::Heading(1)));
    assert!(kinds.contains(&BlockKind::Paragraph));
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| **kind == BlockKind::ListItem)
            .count(),
        2
    );
    assert!(kinds.contains(&BlockKind::TableRow));
    assert!(kinds.contains(&BlockKind::Footnote));

    let paragraph = book
        .blocks
        .iter()
        .find(|block| block.kind == BlockKind::Paragraph && block_text(block).contains("formatted"))
        .expect("fixture should contain the body paragraph");
    assert!(paragraph.text_runs.len() > 1);
    assert!(paragraph.inline_marks.iter().any(|mark| mark.kind == "em"));
    assert!(paragraph.inline_marks.iter().any(|mark| mark.kind == "a"));
    assert!(
        paragraph
            .text_runs
            .iter()
            .any(|run| run.text.contains("<m"))
    );

    let table_row_text = book
        .blocks
        .iter()
        .find(|block| {
            block.kind == BlockKind::TableRow
                && block.text_runs.iter().any(|run| run.text.contains("2024"))
        })
        .expect("fixture should contain a numeric table row");
    assert!(
        table_row_text
            .protected_spans
            .iter()
            .any(|span| span.text == "2024")
    );
    assert!(
        table_row_text
            .protected_spans
            .iter()
            .any(|span| span.text == "42%")
    );

    let segments =
        build_segments(&book, &SegmentationConfig::default()).expect("segments should build");
    assert!(!segments.is_empty());
    assert!(segments.iter().any(|segment| {
        segment
            .source
            .blocks
            .iter()
            .any(|block| block.text_runs.len() > 1)
    }));
}

#[test]
fn parses_huge_paragraph_generated_fixture() {
    let huge = (0..1_500)
        .map(|index| format!("word{index}"))
        .collect::<Vec<_>>()
        .join(" ");
    let body = format!("<h1>Huge Fixture</h1><p>{huge}</p>");
    let fixture = create_epub_fixture("huge", &body);

    let book = read_epub(&fixture).expect("huge fixture should parse");
    let paragraph = book
        .blocks
        .iter()
        .find(|block| block.kind == BlockKind::Paragraph && block_text(block).contains("word1499"))
        .expect("fixture should contain a huge paragraph");

    assert_eq!(paragraph.text_runs.len(), 1);
    assert!(paragraph.token_estimate >= 1_500);

    let segments = build_segments(
        &book,
        &SegmentationConfig {
            max_segment_tokens: 200,
            context_tokens: 16,
        },
    )
    .expect("huge fixture should segment");
    assert!(
        segments
            .iter()
            .any(|segment| segment.source.token_estimate >= 1_500)
    );
}

fn create_minimal_epub() -> PathBuf {
    create_epub_fixture("minimal", "<p>Hello from chapter 1.</p>")
}

fn block_text(block: &bookforge_core::ir::Block) -> String {
    block
        .text_runs
        .iter()
        .map(|run| run.text.as_str())
        .collect::<String>()
}

fn create_epub_fixture(name: &str, body: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_nanos();
    let output = std::env::temp_dir().join(format!(
        "bookforge-{name}-fixture-{}-{nanos}.epub",
        std::process::id()
    ));
    let file = File::create(&output).expect("fixture EPUB should be writable");
    let mut writer = ZipWriter::new(file);
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    writer
        .start_file("mimetype", stored)
        .expect("mimetype entry should start");
    writer
        .write_all(b"application/epub+zip")
        .expect("mimetype should write");
    writer
        .start_file("META-INF/container.xml", deflated)
        .expect("container entry should start");
    writer
        .write_all(CONTAINER_XML.as_bytes())
        .expect("container should write");
    writer
        .start_file("OEBPS/content.opf", deflated)
        .expect("package entry should start");
    writer
        .write_all(CONTENT_OPF.as_bytes())
        .expect("package should write");
    writer
        .start_file("OEBPS/nav.xhtml", deflated)
        .expect("nav entry should start");
    writer
        .write_all(NAV_XHTML.as_bytes())
        .expect("nav should write");
    writer
        .start_file("OEBPS/chapter1.xhtml", deflated)
        .expect("chapter entry should start");
    writer
        .write_all(chapter_xhtml(body).as_bytes())
        .expect("chapter should write");
    writer.finish().expect("fixture EPUB should finish");

    output
}

fn chapter_xhtml(body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<head><title>Generated Fixture</title></head>
<body>{body}</body>
</html>"#
    )
}

const CONTAINER_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;

const CONTENT_OPF: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" unique-identifier="bookid" version="3.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="bookid">bookforge-generated-fixture</dc:identifier>
    <dc:title>Generated Bookforge Fixture</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="chapter1" href="chapter1.xhtml" media-type="application/xhtml+xml"/>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
  </manifest>
  <spine>
    <itemref idref="chapter1"/>
  </spine>
</package>"#;

const NAV_XHTML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<head><title>Navigation</title></head>
<body><nav epub:type="toc"><ol><li><a href="chapter1.xhtml">Generated Fixture</a></li></ol></nav></body>
</html>"#;
