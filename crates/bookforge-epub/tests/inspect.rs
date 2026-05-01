use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use bookforge_epub::inspect_epub;

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

fn create_minimal_epub() -> PathBuf {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace_dir = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("crate should be under workspace/crates/bookforge-epub");
    let source_dir = workspace_dir.join("tests/fixtures/minimal-epub-src");
    let output = workspace_dir.join("tests/fixtures/minimal.epub");
    let _ = fs::remove_file(&output);

    let status = Command::new("bsdtar")
        .current_dir(&source_dir)
        .args(["--format", "zip", "-cf"])
        .arg(&output)
        .args(["mimetype", "META-INF", "OEBPS"])
        .status()
        .expect("bsdtar should be available to create test EPUB");

    assert!(status.success(), "bsdtar failed to create minimal.epub");
    output
}
