use std::path::{Path, PathBuf};

use bookforge_core::ir::BlockKind;
use bookforge_epub::{inspect_epub, read_epub};

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

fn create_minimal_epub() -> PathBuf {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace_dir = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("crate should be under workspace/crates/bookforge-epub");
    workspace_dir.join("tests/fixtures/minimal.epub")
}
