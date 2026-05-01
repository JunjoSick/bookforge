use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

use bookforge_core::{
    config::SegmentationConfig,
    ir::{BlockId, BlockKind},
    segment::{BlockTranslation, build_segments},
};
use bookforge_epub::{inspect_epub, read_epub, rebuild_epub};
use zip::ZipArchive;

#[test]
fn inspects_minimal_epub() {
    let fixture = create_minimal_epub();
    let inspection = inspect_epub(&fixture).expect("fixture should inspect successfully");

    assert_eq!(
        inspection.title.as_deref(),
        Some("Minimal Bookforge Fixture")
    );
    assert_eq!(inspection.spine_count, 1);
    assert_eq!(inspection.manifest_count, 3);
    assert_eq!(inspection.xhtml_count, 2);
    assert_eq!(inspection.xhtml_spine_count, 1);
    assert!(inspection.has_nav);
    assert!(!inspection.has_toc);
    assert_eq!(inspection.resource_count, 1);
}

#[test]
fn builds_basic_ir_from_minimal_epub() {
    let fixture = create_minimal_epub();
    let book = read_epub(&fixture).expect("fixture should parse into IR");

    assert_eq!(book.sections.len(), 1);
    assert_eq!(book.blocks.len(), 2);
    assert_eq!(book.sections[0].title.as_deref(), Some("Chapter 1"));
    assert_eq!(book.blocks[0].kind, BlockKind::Heading(1));
    assert_eq!(book.blocks[1].kind, BlockKind::Paragraph);
    assert_eq!(
        book.blocks[1].text_runs[0].text,
        "Hello from a minimal EPUB fixture."
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

    assert_eq!(first.len(), 1);
    assert_eq!(first[0].id, second[0].id);
    assert_eq!(first[0].checksum, second[0].checksum);
    assert_eq!(first[0].section_id.0, "sec_000000");
    assert_eq!(first[0].block_ids.len(), 2);
    assert!(first[0].source.text.contains("Chapter 1"));
    assert!(
        first[0]
            .source
            .text
            .contains("Hello from a minimal EPUB fixture.")
    );
    assert!(first[0].source.token_estimate > 0);
    assert!(first[0].context.before.is_none());
    assert!(first[0].context.after.is_none());
}

#[test]
fn rebuilds_epub_with_patched_xhtml_and_preserved_resources() {
    let fixture = create_minimal_epub();
    let book = read_epub(&fixture).expect("fixture should parse into IR");
    let output =
        std::env::temp_dir().join(format!("bookforge-rebuilt-{}.epub", std::process::id()));
    let _ = std::fs::remove_file(&output);

    rebuild_epub(
        &book,
        &[
            BlockTranslation {
                block_id: BlockId("b_000000".to_string()),
                text: "Capitolo 1".to_string(),
            },
            BlockTranslation {
                block_id: BlockId("b_000001".to_string()),
                text: "Ciao da un EPUB minimo.".to_string(),
            },
        ],
        &output,
    )
    .expect("EPUB should rebuild");

    let inspection = inspect_epub(&output).expect("rebuilt EPUB should inspect");
    assert_eq!(inspection.spine_count, 1);
    assert_eq!(inspection.manifest_count, 3);

    let mut archive = ZipArchive::new(File::open(&output).expect("rebuilt EPUB should exist"))
        .expect("rebuilt EPUB should be a zip");
    let mut chapter = String::new();
    archive
        .by_name("OEBPS/chapter1.xhtml")
        .expect("chapter should be present")
        .read_to_string(&mut chapter)
        .expect("chapter should be UTF-8");

    assert!(chapter.contains("Capitolo 1"));
    assert!(chapter.contains("Ciao da un EPUB minimo."));
    assert!(chapter.contains("style.css"));
}

fn create_minimal_epub() -> PathBuf {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace_dir = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("crate should be under workspace/crates/bookforge-epub");
    workspace_dir.join("tests/fixtures/minimal.epub")
}
