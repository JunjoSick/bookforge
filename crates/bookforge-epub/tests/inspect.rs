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
    RebuildOptions, inspect_epub, read_epub, rebuild_epub, rebuild_epub_with_options, text_coverage,
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
fn non_ascii_entry_names_resolve_through_inspect_read_and_rebuild() {
    // zip 9 decodes entry names on access instead of at parse time; a UTF-8
    // name must still match its manifest href on every archive path.
    let href = "capítulo-東京.xhtml";
    let fixture =
        create_epub_fixture_with_chapter("non-ascii-name", href, "<p>Hello from chapter 1.</p>");

    let inspection = inspect_epub(&fixture).expect("non-ASCII spine entry should be found");
    assert_eq!(inspection.xhtml_spine_count, 1);

    let book = read_epub(&fixture).expect("non-ASCII entry should parse into IR");
    let body_block = book
        .blocks
        .iter()
        .find(|block| block_text(block) == "Hello from chapter 1.")
        .expect("body paragraph should be extracted");

    let output = std::env::temp_dir().join(format!(
        "bookforge-rebuilt-non-ascii-{}.epub",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&output);
    rebuild_epub(
        &book,
        &[BlockTranslation {
            block_id: body_block.id.clone(),
            text: "Ciao da un capitolo non ASCII.".to_string(),
        }],
        &output,
    )
    .expect("EPUB with a non-ASCII entry should rebuild");

    let mut archive = ZipArchive::new(File::open(&output).expect("rebuilt EPUB should exist"))
        .expect("rebuilt EPUB should be a zip");
    let mut chapter = String::new();
    archive
        .by_name(&format!("OEBPS/{href}"))
        .expect("non-ASCII entry name should survive the rebuild")
        .read_to_string(&mut chapter)
        .expect("chapter should be UTF-8");
    assert!(chapter.contains("Ciao da un capitolo non ASCII."));
}

#[test]
fn packed_notes_extract_and_rebuild_at_matching_dom_paths() {
    let fixture = create_epub_fixture(
        "packed-notes",
        r#"<p id="notes" class="notes"><a id="n1"/><a href="chapter1.xhtml#r1">1</a> First <em>note</em>. <a id="n2"/><a href="chapter1.xhtml#r2">2</a> Second note.</p><p>After notes.</p>"#,
    );
    let book = read_epub(&fixture).unwrap();
    let notes: Vec<_> = book
        .blocks
        .iter()
        .filter(|block| {
            let text = block_text(block);
            text.contains("First") || text.contains("Second")
        })
        .collect();
    assert_eq!(notes.len(), 2);
    let translations: Vec<_> = notes
        .iter()
        .map(|block| BlockTranslation {
            block_id: block.id.clone(),
            text: block_text(block)
                .replace("First", "Primo")
                .replace("Second", "Secondo"),
        })
        .collect();
    let output = fixture.with_extension("rebuilt.epub");
    rebuild_epub(&book, &translations, &output).unwrap();
    let mut archive = ZipArchive::new(File::open(&output).unwrap()).unwrap();
    let mut chapter = String::new();
    archive
        .by_name("OEBPS/chapter1.xhtml")
        .unwrap()
        .read_to_string(&mut chapter)
        .unwrap();
    assert!(chapter.contains("Primo"));
    assert!(chapter.contains("Secondo"));
    assert!(chapter.contains("<em>note</em>"));
    assert!(chapter.contains("After notes."));
    for id in ["notes", "n1", "n2"] {
        assert_eq!(chapter.matches(&format!("id=\"{id}\"")).count(), 1);
    }
    for href in ["chapter1.xhtml#r1", "chapter1.xhtml#r2"] {
        assert!(chapter.contains(href));
    }
    let rebuilt = read_epub(&output).unwrap();
    assert_eq!(book.blocks.len(), rebuilt.blocks.len());
    for translation in translations {
        assert!(
            rebuilt
                .blocks
                .iter()
                .any(|block| block_text(block) == translation.text)
        );
    }
    std::fs::remove_file(fixture).unwrap();
    std::fs::remove_file(output).unwrap();
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
fn manifest_documents_outside_the_spine_are_extracted_once_and_translated() {
    let package = CONTENT_OPF.replace(
        "</manifest>",
        r#"  <item id="endnotes" href="endnotes.xhtml" media-type="application/xhtml+xml"/>
    <item id="alias" href="./chapter1.xhtml" media-type="application/xhtml+xml"/>
    <item id="data" href="data.xml" media-type="application/xml"/>
    <item id="absent" href="absent.xhtml" media-type="application/xhtml+xml"/>
  </manifest>"#,
    );
    let fixture = create_epub_fixture_with_package(
        "non-spine-documents",
        "chapter1.xhtml",
        "<p>Body paragraph.</p>",
        &package,
        &[
            (
                "OEBPS/endnotes.xhtml",
                "<html><body><h2>Notes</h2><p>A note reached only by a link.</p></body></html>",
            ),
            (
                "OEBPS/data.xml",
                "<records><text>Not prose.</text></records>",
            ),
        ],
    );

    let book = read_epub(&fixture).unwrap();
    assert_eq!(
        book.blocks
            .iter()
            .filter(|block| block_text(block) == "Body paragraph.")
            .count(),
        1,
        "an aliased manifest item must not duplicate the chapter"
    );
    assert!(
        !book
            .blocks
            .iter()
            .any(|block| block_text(block) == "Not prose."),
        "generic XML outside the spine is not prose"
    );
    let note = book
        .blocks
        .iter()
        .find(|block| block_text(block) == "A note reached only by a link.")
        .expect("non-spine XHTML must be extracted");
    let last = book.sections.last().unwrap();
    assert_eq!(last.href, "OEBPS/endnotes.xhtml");
    assert_eq!(last.id.0, "sec_resource_000000");
    assert_eq!(last.title.as_deref(), Some("Notes"));

    let coverage = text_coverage(&fixture).unwrap();
    assert_eq!(
        coverage
            .files
            .iter()
            .map(|file| file.href.as_str())
            .collect::<Vec<_>>(),
        ["OEBPS/chapter1.xhtml", "OEBPS/endnotes.xhtml"],
        "coverage follows extraction order and leaves navigation out"
    );
    assert_eq!(coverage.percent(), 100.0);

    rebuild_epub(
        &book,
        &[BlockTranslation {
            block_id: note.id.clone(),
            text: "Una nota raggiunta solo da un collegamento.".to_string(),
        }],
        &fixture,
    )
    .unwrap();
    let rebuilt = read_epub(&fixture).unwrap();
    assert!(
        rebuilt
            .blocks
            .iter()
            .any(|block| { block_text(block) == "Una nota raggiunta solo da un collegamento." })
    );
    assert!(
        !rebuilt
            .blocks
            .iter()
            .any(|block| block_text(block) == "A note reached only by a link.")
    );
}

#[test]
fn opaque_inline_math_svg_script_and_style_survive_translation_byte_for_byte() {
    let opaque = [
        r#"<math xmlns="http://www.w3.org/1998/Math/MathML"><mrow><mi>n</mi><mo>&gt;</mo><mn>3</mn></mrow></math>"#,
        r#"<svg xmlns="http://www.w3.org/2000/svg"><text>Axis label</text></svg>"#,
        "<script><![CDATA[const caption = 'kept';]]></script>",
        "<style>.mark { color: blue; }</style>",
    ];
    let body = format!(
        "<p>Where {} holds, <em>see</em>{}{}{}.</p>",
        opaque[0], opaque[1], opaque[2], opaque[3]
    );
    let fixture = create_epub_fixture("opaque-inline", &body);
    let book = read_epub(&fixture).unwrap();
    let paragraph = book
        .blocks
        .iter()
        .find(|block| block_text(block).starts_with("Where"))
        .unwrap();
    assert_eq!(
        block_text(paragraph),
        "Where <r1/> holds, <m2>see</m2><r3/><r4/><r5/>."
    );

    rebuild_epub(
        &book,
        &[BlockTranslation {
            block_id: paragraph.id.clone(),
            text: "Dove vale <r1/>, <m2>vedi</m2><r3/><r4/><r5/>.".to_string(),
        }],
        &fixture,
    )
    .unwrap();

    let chapter = entry_text(&fixture, "OEBPS/chapter1.xhtml");
    assert!(chapter.contains("Dove vale"), "{chapter}");
    assert!(chapter.contains("<em>vedi</em>"), "{chapter}");
    for original in opaque {
        assert!(
            chapter.contains(original),
            "opaque content changed: {original}\n{chapter}"
        );
    }
}

#[test]
fn mixed_content_round_trips_without_missing_or_duplicated_text() {
    let bodies = [
        "Red &amp; Blue<!--cut-->Green<![CDATA[ Gold]]><p>Pink<br/>Teal</p>Plum&#33;",
        "<section>Red &amp; Blue<span>Green<em>Gold</em></span><![CDATA[Pink]]>&#33;<br/>Teal</section>",
        "<div>Red<p>Blue<em>Green</em></p>Gold<math><mi>z</mi></math>Pink</div>",
        "<div><span>Red</span>Blue<!--cut-->Green&#33;<svg><text>Kept label</text></svg>Gold</div>",
        "<blockquote><p>Red</p><p>Blue</p></blockquote>Green<pre>Code</pre>Gold",
    ];
    let words = ["Red", "Blue", "Green", "Gold", "Pink", "Teal", "Plum"];
    for body in bodies {
        let fixture = create_epub_fixture("mixed-content-parity", body);
        let book = read_epub(&fixture).unwrap();
        let expected = book
            .blocks
            .iter()
            .map(|block| {
                words.iter().fold(block_text(block), |text, word| {
                    text.replace(word, &format!("New{word}"))
                })
            })
            .collect::<Vec<_>>();
        let translations = book
            .blocks
            .iter()
            .zip(&expected)
            .map(|(block, text)| BlockTranslation {
                block_id: block.id.clone(),
                text: text.clone(),
            })
            .collect::<Vec<_>>();

        rebuild_epub(&book, &translations, &fixture).unwrap();
        let rebuilt = read_epub(&fixture).unwrap();
        assert_eq!(
            rebuilt.blocks.iter().map(block_text).collect::<Vec<_>>(),
            expected,
            "mixed content changed: {body}"
        );
    }
}

#[test]
fn loose_text_keeps_its_spacing_next_to_inline_siblings() {
    let fixture = create_epub_fixture(
        "loose-boundary-spaces",
        "<section>One <em>two</em> three&nbsp;<strong>four</strong>.</section>",
    );
    let book = read_epub(&fixture).unwrap();
    let replacements = [
        ("One", "Uno"),
        ("two", "due"),
        ("three", "tre"),
        ("four", "quattro"),
    ];
    let translations = book
        .blocks
        .iter()
        .filter_map(|block| {
            let source = block_text(block);
            replacements
                .iter()
                .find(|(original, _)| *original == source)
                .map(|(_, translated)| BlockTranslation {
                    block_id: block.id.clone(),
                    text: (*translated).to_string(),
                })
        })
        .collect::<Vec<_>>();
    assert_eq!(translations.len(), replacements.len());

    rebuild_epub(&book, &translations, &fixture).unwrap();

    let chapter = entry_text(&fixture, "OEBPS/chapter1.xhtml");
    assert!(
        chapter.contains("<section>Uno <em>due</em> tre\u{a0}<strong>quattro</strong>.</section>"),
        "loose words lost their separators: {chapter}"
    );
}

/// Rebuild `book` into its own source path and assert it fails without
/// touching the existing file.
fn assert_rebuild_rejected(
    book: &bookforge_core::ir::Book,
    translations: &[BlockTranslation],
    fixture: &std::path::Path,
    expected: &str,
) {
    let original = std::fs::read(fixture).unwrap();
    let error = rebuild_epub(book, translations, fixture)
        .expect_err("an unusable translation request must not report success");
    assert!(
        error.to_string().contains(expected),
        "expected '{expected}' in: {error}"
    );
    assert_eq!(
        std::fs::read(fixture).unwrap(),
        original,
        "a rejected rebuild must leave the existing output untouched"
    );
}

fn body_block(book: &bookforge_core::ir::Book) -> bookforge_core::ir::Block {
    book.blocks
        .iter()
        .find(|block| block_text(block) == "Hello from chapter 1.")
        .expect("body paragraph should be extracted")
        .clone()
}

#[test]
fn rebuild_rejects_translations_for_unknown_or_repeated_blocks() {
    let fixture = create_minimal_epub();
    let book = read_epub(&fixture).unwrap();
    let paragraph = body_block(&book);

    assert_rebuild_rejected(
        &book,
        &[BlockTranslation {
            block_id: bookforge_core::ir::BlockId("b_nowhere".to_string()),
            text: "Ciao.".to_string(),
        }],
        &fixture,
        "unknown block 'b_nowhere'",
    );
    assert_rebuild_rejected(
        &book,
        &[
            BlockTranslation {
                block_id: paragraph.id.clone(),
                text: "Prima.".to_string(),
            },
            BlockTranslation {
                block_id: paragraph.id.clone(),
                text: "Seconda.".to_string(),
            },
        ],
        &fixture,
        "duplicate block",
    );
}

#[test]
fn rebuild_rejects_ambiguous_ir_identifiers() {
    let fixture = create_minimal_epub();
    let book = read_epub(&fixture).unwrap();
    let paragraph = body_block(&book);
    let translation = [BlockTranslation {
        block_id: paragraph.id.clone(),
        text: "Ciao.".to_string(),
    }];

    let mut repeated_block = book.clone();
    repeated_block.blocks.push(paragraph.clone());
    assert_rebuild_rejected(&repeated_block, &translation, &fixture, "duplicate block");

    let mut repeated_section = book.clone();
    let section = repeated_section
        .sections
        .iter()
        .find(|section| section.id == paragraph.section_id)
        .unwrap()
        .clone();
    repeated_section.sections.push(section);
    assert_rebuild_rejected(
        &repeated_section,
        &translation,
        &fixture,
        "duplicate section",
    );
}

#[test]
fn rebuild_rejects_targets_missing_from_the_ir_or_the_archive() {
    let fixture = create_minimal_epub();
    let book = read_epub(&fixture).unwrap();
    let paragraph = body_block(&book);
    let translation = [BlockTranslation {
        block_id: paragraph.id.clone(),
        text: "Ciao.".to_string(),
    }];

    let mut orphaned = book.clone();
    orphaned
        .blocks
        .iter_mut()
        .find(|block| block.id == paragraph.id)
        .unwrap()
        .section_id = bookforge_core::ir::SectionId("sec_gone".to_string());
    assert_rebuild_rejected(&orphaned, &translation, &fixture, "missing section");

    let mut misplaced = book.clone();
    misplaced
        .sections
        .iter_mut()
        .find(|section| section.id == paragraph.section_id)
        .unwrap()
        .href = "OEBPS/gone.xhtml".to_string();
    assert_rebuild_rejected(&misplaced, &translation, &fixture, "not in the archive");

    let mut dangling = book.clone();
    dangling
        .blocks
        .iter_mut()
        .find(|block| block.id == paragraph.id)
        .unwrap()
        .dom_path
        .0
        .push(99);
    assert_rebuild_rejected(
        &dangling,
        &translation,
        &fixture,
        "not found in the document",
    );
}

#[test]
fn rebuild_rejects_two_translations_for_one_element() {
    let fixture = create_epub_fixture("colliding-patches", "<p>Left.</p><p>Right.</p>");
    let mut book = read_epub(&fixture).unwrap();
    let left = book
        .blocks
        .iter()
        .find(|block| block_text(block) == "Left.")
        .unwrap()
        .clone();
    let right = book
        .blocks
        .iter_mut()
        .find(|block| block_text(block) == "Right.")
        .unwrap();
    right.dom_path = left.dom_path.clone();
    let right_id = right.id.clone();

    assert_rebuild_rejected(
        &book,
        &[
            BlockTranslation {
                block_id: left.id,
                text: "Sinistra.".to_string(),
            },
            BlockTranslation {
                block_id: right_id,
                text: "Destra.".to_string(),
            },
        ],
        &fixture,
        "same DOM path",
    );
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
            .all(|span| span.text != "2024"),
        "bare marker reference text is owned by marker validation"
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

fn entry_text(epub: &std::path::Path, name: &str) -> String {
    let mut archive = ZipArchive::new(File::open(epub).expect("EPUB should open")).expect("zip");
    let mut text = String::new();
    archive
        .by_name(name)
        .unwrap_or_else(|error| panic!("entry {name} missing: {error}"))
        .read_to_string(&mut text)
        .expect("entry should be UTF-8");
    text
}

fn create_epub_fixture(name: &str, body: &str) -> PathBuf {
    create_epub_fixture_with_chapter(name, "chapter1.xhtml", body)
}

fn create_epub_fixture_with_chapter(name: &str, chapter_href: &str, body: &str) -> PathBuf {
    create_epub_fixture_with_package(
        name,
        chapter_href,
        body,
        &CONTENT_OPF.replace("chapter1.xhtml", chapter_href),
        &[],
    )
}

fn create_epub_fixture_with_package(
    name: &str,
    chapter_href: &str,
    body: &str,
    package: &str,
    extra_entries: &[(&str, &str)],
) -> PathBuf {
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
        .write_all(package.as_bytes())
        .expect("package should write");
    writer
        .start_file("OEBPS/nav.xhtml", deflated)
        .expect("nav entry should start");
    writer
        .write_all(NAV_XHTML.replace("chapter1.xhtml", chapter_href).as_bytes())
        .expect("nav should write");
    writer
        .start_file(format!("OEBPS/{chapter_href}"), deflated)
        .expect("chapter entry should start");
    writer
        .write_all(chapter_xhtml(body).as_bytes())
        .expect("chapter should write");
    for (entry, text) in extra_entries {
        writer
            .start_file(*entry, deflated)
            .expect("extra entry should start");
        writer
            .write_all(text.as_bytes())
            .expect("extra entry should write");
    }
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
