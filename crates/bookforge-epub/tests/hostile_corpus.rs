//! Hostile-input corpus for the EPUB reader/writer boundary
//! (audit investment #4 / EPUB-18).
//!
//! Every fixture is generated in-test (no binary assets). The suite drives
//! zip-bomb lies, oversized nesting, mismatched marker payloads, truncation
//! at every byte boundary, manifest/path lies, nav-less EPUB3, odd
//! percent-encodings, mixed-case extensions, a synthetic EPUB2/OEBPS book,
//! and the injectable archive-limits hook (tiny explicit bounds so bomb
//! cases run in milliseconds instead of needing ≥64 MiB fixtures).
//!
//! All inputs are fully deterministic: fixed strings, fixed seeds of
//! pseudo-content, no runtime entropy.

use std::{
    fs::File,
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
};

use bookforge_core::{config::BilingualMode, segment::BlockTranslation};
use bookforge_epub::{
    RebuildOptions,
    archive_limits::{ArchiveLimits, read_archive_text, validate_archive_metadata},
    inspect_epub, read_epub, rebuild_epub_with_options, text_coverage,
};
use quick_xml::{Reader, events::Event};
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

// ---------------------------------------------------------------------------
// Fixture plumbing
// ---------------------------------------------------------------------------

const MIMETYPE: &[u8] = b"application/epub+zip";

/// Generic deterministic EPUB writer: one archive with the caller's exact
/// entries. `entries` are appended after mimetype/container; the OPF lives
/// wherever the container's rootfile says it does.
fn write_archive(path: &Path, entries: &[(&str, Vec<u8>)]) {
    let file = File::create(path).expect("fixture should be creatable");
    let mut zip = ZipWriter::new(file);
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    zip.start_file("mimetype", stored)
        .expect("mimetype should start");
    zip.write_all(MIMETYPE).expect("mimetype should write");
    for (name, bytes) in entries {
        zip.start_file(*name, deflated)
            .unwrap_or_else(|error| panic!("entry {name} should start: {error}"));
        zip.write_all(bytes.as_slice())
            .unwrap_or_else(|error| panic!("entry {name} should write: {error}"));
    }
    zip.finish().expect("fixture should finish");
}

fn utf8(value: &str) -> Vec<u8> {
    value.as_bytes().to_vec()
}

const CONTAINER: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;

fn container_with(rootfile: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="{rootfile}" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#
    )
}

fn chapter(title: &str, body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<head><title>{title}</title></head>
<body>{body}</body>
</html>"#
    )
}

fn tmp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bf-hostile-{label}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir should create");
    dir
}

fn entry_bytes(path: &Path, name: &str) -> Vec<u8> {
    let mut archive =
        ZipArchive::new(File::open(path).expect("fixture should open")).expect("zip should parse");
    let mut bytes = Vec::new();
    archive
        .by_name(name)
        .unwrap_or_else(|error| panic!("entry {name} missing: {error}"))
        .read_to_end(&mut bytes)
        .expect("entry should read");
    bytes
}

fn entry_text(path: &Path, name: &str) -> String {
    String::from_utf8(entry_bytes(path, name)).expect("entry should be UTF-8")
}

fn is_well_formed(xhtml: &str) -> bool {
    let mut reader = Reader::from_str(xhtml);
    reader.config_mut().trim_text(false);
    loop {
        match reader.read_event() {
            Ok(Event::Eof) => return true,
            Ok(_) => continue,
            Err(_) => return false,
        }
    }
}

fn expect_read_error(epub: &Path) -> String {
    match read_epub(epub) {
        Ok(_) => panic!("expected read_epub({}) to fail cleanly", epub.display()),
        Err(error) => error.to_string(),
    }
}

use zip::ZipArchive;

// ---------------------------------------------------------------------------
// Zip-bomb lies against injected tiny limits (deliverable 4 hook)
// ---------------------------------------------------------------------------

fn tiny_limits(ratio_entries: bool) -> ArchiveLimits {
    ArchiveLimits {
        max_entries: 8,
        max_entry_uncompressed_size: if ratio_entries { 512 * 1024 } else { 64 * 1024 },
        max_total_uncompressed_size: 96 * 1024,
        max_entry_compression_ratio: 200,
        max_archive_compression_ratio: 100,
    }
}

/// Limits for declared-size-liar fixtures whose payloads are highly
/// compressible (`x`/`z` runs): the ratio ceiling must stay non-binding so
/// the size caps are what actually trip.
fn liar_limits() -> ArchiveLimits {
    ArchiveLimits {
        max_entry_compression_ratio: 100_000,
        max_archive_compression_ratio: 100_000,
        ..tiny_limits(false)
    }
}

#[test]
fn injectable_limits_reject_entry_count_lies() {
    let dir = tmp_dir("bomb-count");
    let epub = dir.join("count.epub");
    let mut entries: Vec<(String, Vec<u8>)> =
        vec![("META-INF/container.xml".into(), utf8(CONTAINER))];
    for index in 0..9 {
        entries.push((format!("blob{index}"), vec![b'x'; 16]));
    }
    let refs = entries
        .iter()
        .map(|(name, bytes)| (name.as_str(), bytes.clone()))
        .collect::<Vec<_>>();
    write_archive(&epub, &refs);

    let mut archive = ZipArchive::new(File::open(&epub).expect("open")).expect("zip");
    let error = validate_archive_metadata(&mut archive, tiny_limits(false))
        .expect_err("9 entries must exceed max_entries=8");
    assert!(
        error.to_string().contains("entry count limit exceeded"),
        "unexpected error: {error}"
    );
}

#[test]
fn injectable_limits_catch_declared_size_and_ratio_lies() {
    let dir = tmp_dir("bomb-meta");

    // Declared uncompressed size over the per-entry cap.
    let epub = dir.join("size.epub");
    write_archive(
        &epub,
        &[
            ("META-INF/container.xml", utf8(CONTAINER)),
            ("OEBPS/content.opf", utf8(OPF_MINIMAL)),
            ("fat.bin", vec![b'a'; 128 * 1024]),
        ],
    );
    let mut archive = ZipArchive::new(File::open(&epub).expect("open")).expect("zip");
    let error = validate_archive_metadata(&mut archive, tiny_limits(false))
        .expect_err("declared 128 KiB entry must exceed the 64 KiB cap");
    assert!(
        error
            .to_string()
            .contains("per-entry uncompressed size limit"),
        "unexpected error: {error}"
    );

    // Declared ratio lie: tiny claimed compressed size against a large
    // claimed expansion — metadata stage rejects before any bytes move.
    let epub = dir.join("ratio.epub");
    write_archive(
        &epub,
        &[
            ("META-INF/container.xml", utf8(CONTAINER)),
            ("OEBPS/content.opf", utf8(OPF_MINIMAL)),
            ("sneaky.xml", utf8("<root/>")),
        ],
    );
    let mut bytes = std::fs::read(&epub).expect("fixture should read");
    {
        let last_central = bytes
            .windows(4)
            .rposition(|w| w == b"PK\x01\x02")
            .expect("central header");
        // Only inflate the LAST regular entry's declared sizes (the sneaky
        // entry). Offsets: +20 compressed size, +24 uncompressed size.
        bytes[last_central + 20..last_central + 24].copy_from_slice(&1u32.to_le_bytes());
        bytes[last_central + 24..last_central + 28].copy_from_slice(&(256 * 1024u32).to_le_bytes());
    }
    std::fs::write(&epub, &bytes).expect("patched fixture should write");
    let mut archive = ZipArchive::new(File::open(&epub).expect("open")).expect("zip");
    let error = validate_archive_metadata(&mut archive, tiny_limits(true))
        .expect_err("declared 256 KiB from 1 B must trip the 200:1 ratio");
    assert!(
        error.to_string().contains("compression ratio limit"),
        "unexpected error: {error}"
    );
}

#[test]
fn bounded_read_catches_declared_sizes_that_lie_about_expansion() {
    let dir = tmp_dir("bomb-read");
    let epub = dir.join("lie.epub");
    // Stored payload expands far beyond what the central directory claims;
    // with an injected 1 KiB cap the bounded read stops after 1 KiB + 1.
    let big = vec![b'x'; 64 * 1024];
    write_deflated_fixture(&dir.join("liar-src.zip"), "liar.bin", &big);
    relabel_declared_sizes(&dir.join("liar-src.zip"), &epub);

    let mut archive = ZipArchive::new(File::open(&epub).expect("open")).expect("zip");
    let limits = ArchiveLimits {
        max_entry_uncompressed_size: 1024,
        ..liar_limits()
    };
    let mut budget =
        validate_archive_metadata(&mut archive, limits).expect("small declared sizes fit caps");

    let mut file = archive.by_name("liar.bin").expect("entry exists");
    let compressed = file.compressed_size();
    let error = budget
        .read_entry(&mut file, "liar.bin", compressed)
        .expect_err("actual 64 KiB expansion must be caught");
    assert!(
        error.to_string().contains("expanded data exceeds"),
        "unexpected error: {error}"
    );
}

fn write_deflated_fixture(path: &Path, name: &str, payload: &[u8]) {
    let file = File::create(path).expect("raw fixture should create");
    let mut zip = ZipWriter::new(file);
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    zip.start_file(name, deflated).expect("entry should start");
    zip.write_all(payload).expect("payload should write");
    zip.finish().expect("fixture should finish");
}

/// Rewrite every central-directory record down to 16-byte declared sizes
/// (the source fixture holds exactly the liar entry) and save at `target`.
fn relabel_declared_sizes(source: &Path, target: &Path) {
    let mut bytes = std::fs::read(source).expect("source should read");
    let needle = b"PK\x01\x02";
    let mut cursor = 0usize;
    while let Some(found) = bytes[cursor..]
        .windows(needle.len())
        .position(|window| window == needle)
    {
        let offset = cursor + found;
        // Only the declared UNCOMPRESSED size is shrunk: touching the
        // declared compressed size would desync zip's own inflation
        // bookkeeping and mask the lie behind a CRC error instead of a
        // clean budget rejection.
        bytes[offset + 24..offset + 28].copy_from_slice(&16u32.to_le_bytes());
        cursor = offset + needle.len();
    }
    std::fs::write(target, bytes).expect("patched fixture should write");
}

#[test]
fn bounded_budget_tracks_total_expansion_across_entries() {
    // Two deflated entries whose central declarations say 16 bytes but that
    // actually expand to 48 KiB each. The metadata stage passes (declared
    // totals fit the tiny caps), so the running total is enforced at READ
    // time: entry one fits, entry two crosses the remaining budget.
    let half: Vec<u8> = vec![b'z'; 48 * 1024];
    let dir = tmp_dir("bomb-total");
    let source = dir.join("half-src.zip");
    let file = File::create(&source).expect("create");
    let mut zip = ZipWriter::new(file);
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    zip.start_file("a.bin", deflated).expect("start");
    zip.write_all(&half).expect("write");
    zip.start_file("b.bin", deflated).expect("start");
    zip.write_all(&half).expect("write");
    zip.finish().expect("finish");
    let epub = dir.join("liars.epub");
    relabel_declared_sizes(&source, &epub);

    let mut archive = ZipArchive::new(File::open(&epub).expect("open")).expect("zip");
    let limits = ArchiveLimits {
        max_entry_uncompressed_size: 64 * 1024,
        max_total_uncompressed_size: 80 * 1024,
        ..ArchiveLimits {
            max_entry_compression_ratio: 100_000,
            max_archive_compression_ratio: 100_000,
            ..tiny_limits(true)
        }
    };
    let mut budget =
        validate_archive_metadata(&mut archive, limits).expect("16-byte declarations fit");

    for name in ["a.bin", "b.bin"] {
        let mut file = archive.by_name(name).expect("entry exists");
        let compressed = file.compressed_size();
        let outcome = budget.read_entry(&mut file, name, compressed);
        if name == "b.bin" {
            let error = outcome.expect_err("second read crosses the running total");
            assert!(
                error
                    .to_string()
                    .contains("total uncompressed size limit exceeded"),
                "unexpected error: {error}"
            );
        } else {
            let bytes = outcome.expect("first read fits");
            assert_eq!(bytes.len(), 48 * 1024);
        }
    }

    // Direct-construction positive control for the injectable hook: an
    // explicitly-limited budget through `validate_archive_metadata` +
    // `read_archive_text`, with no production-sized fixture anywhere.
    let ok_dir = tmp_dir("bomb-total");
    write_deflated_fixture(&ok_dir.join("ok.zip"), "text.txt", &half[..64]);
    let mut archive =
        ZipArchive::new(File::open(ok_dir.join("ok.zip")).expect("open")).expect("zip parses");
    let limits = ArchiveLimits {
        max_entry_uncompressed_size: 1024,
        max_total_uncompressed_size: 2048,
        ..tiny_limits(true)
    };
    let mut budget = validate_archive_metadata(&mut archive, limits)
        .expect("declared 16-byte lie still fits these caps");
    let text = read_archive_text(&mut archive, &mut budget, "text.txt")
        .expect("bounded text read succeeds under injected bounds");
    assert!(text.contains('z'));
}

const OPF_MINIMAL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" unique-identifier="uid" version="3.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="uid">minimal</dc:identifier>
    <dc:title>Minimal</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest/>
  <spine/>
</package>"#;

// ---------------------------------------------------------------------------
// Deep marker nesting → graceful skip (never a crash)
// ---------------------------------------------------------------------------

#[test]
fn deeply_nested_markers_are_skipped_gracefully_and_preserve_source() {
    let depth = 40usize; // beyond MAX_MARKER_DEPTH (32)
    let mut inner = String::from("kernel.");
    for level in 0..depth {
        inner = format!("<b id=\"d{level}\">{inner}</b>");
    }
    let body = format!("<p id=\"deep\">{inner}</p><p>sibling payload.</p>");

    let dir = tmp_dir("deep-nesting");
    let input = dir.join("in.epub");
    write_archive(
        &input,
        &[
            ("META-INF/container.xml", utf8(CONTAINER)),
            ("OEBPS/content.opf", utf8(&opf_single("deep-fixture"))),
            ("OEBPS/ch1.xhtml", utf8(&chapter("Deep Nesting", &body))),
        ],
    );

    let book = read_epub(&input).expect("deeply nested markup must still parse");
    let deep_block = book
        .blocks
        .iter()
        .find(|block| marked(block).contains("kernel"))
        .expect("the deeply nested paragraph must be extracted")
        .clone();
    let deep_translation = format!("[Tr]{}", marked(&deep_block));
    assert!(
        deep_translation.matches("<m").count() > 16,
        "fixture must actually carry deep nesting"
    );
    let sibling_block = book
        .blocks
        .iter()
        .find(|block| marked(block).contains("sibling"))
        .expect("the healthy sibling paragraph must be extracted")
        .clone();
    let translations = vec![
        BlockTranslation {
            block_id: deep_block.id.clone(),
            text: deep_translation,
        },
        BlockTranslation {
            block_id: sibling_block.id.clone(),
            text: format!("[Tr]{}", marked(&sibling_block)),
        },
    ];

    let output = dir.join("out.epub");
    rebuild_epub_with_options(
        &book,
        &translations,
        &output,
        &RebuildOptions {
            mode: BilingualMode::Replace,
            ..RebuildOptions::default()
        },
    )
    .expect("rebuild must degrade gracefully instead of failing");

    let rebuilt = entry_text(&output, "OEBPS/ch1.xhtml");
    assert!(
        is_well_formed(&rebuilt),
        "skipped deep block must leave the document well-formed"
    );
    // The over-deep translation was refused: the ORIGINAL block bytes are
    // preserved verbatim and no prefix appears for it...
    let deep_source_slice = format!("<p id=\"deep\">{inner}</p>");
    assert!(
        rebuilt.contains(deep_source_slice.as_str()),
        "over-depth block must preserve original bytes"
    );
    // ...while the healthy sibling IS translated.
    assert_eq!(
        rebuilt.matches("[Tr]").count(),
        1,
        "exactly the sibling block carries an applied translation: {rebuilt}"
    );
}

// ---------------------------------------------------------------------------
// Mismatched / overlapping marker payloads degrade to preserved source
// ---------------------------------------------------------------------------

#[test]
fn mismatched_marker_translations_preserve_original_blocks() {
    let body = r#"<p class="keep">alpha <em>beta</em> gamma</p><p class="plain">delta</p>"#;
    let dir = tmp_dir("marker-mismatch");
    let input = dir.join("in.epub");
    write_archive(
        &input,
        &[
            ("META-INF/container.xml", utf8(CONTAINER)),
            ("OEBPS/content.opf", utf8(&opf_single("mismatch"))),
            ("OEBPS/ch1.xhtml", utf8(&chapter("Mismatch", body))),
        ],
    );

    let book = read_epub(&input).expect("body should parse");
    let mismatched = book
        .blocks
        .iter()
        .find(|block| marked(block) == "alpha <m1>beta</m1> gamma")
        .expect("em paragraph carries m1");
    let healthy = book
        .blocks
        .iter()
        .find(|block| marked(block) == "delta")
        .expect("the delta paragraph must be extracted verbatim");

    let translations = vec![
        BlockTranslation {
            block_id: mismatched.id.clone(),
            // Unbalanced: closes m1 that was never opened in the payload.
            text: "[Tr]tradotto</m1>".to_string(),
        },
        BlockTranslation {
            block_id: healthy.id.clone(),
            text: "[Tr]deltra".to_string(),
        },
    ];

    let output = dir.join("out.epub");
    rebuild_epub_with_options(
        &book,
        &translations,
        &output,
        &RebuildOptions {
            mode: BilingualMode::Replace,
            ..RebuildOptions::default()
        },
    )
    .expect("rebuild must survive invalid payloads");

    let rebuilt = entry_text(&output, "OEBPS/ch1.xhtml");
    assert!(is_well_formed(&rebuilt));
    // The mismatched block kept its SOURCE bytes (including its original
    // inner markup) and shows no applied prefix.
    assert!(
        rebuilt.contains(r#"<p class="keep">alpha <em>beta</em> gamma</p>"#),
        "invalid payload must preserve original block: {rebuilt}"
    );
    // The sibling translation applied normally.
    assert!(
        rebuilt.contains("[Tr]deltra"),
        "healthy block must still apply: {rebuilt}"
    );
}

fn marked(block: &bookforge_core::ir::Block) -> String {
    block
        .text_runs
        .iter()
        .map(|run| run.text.as_str())
        .collect::<String>()
}

fn opf_single(title: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" unique-identifier="uid" version="3.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="uid">{title}</dc:identifier>
    <dc:title>{title}</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="ch1" href="ch1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="ch1"/>
  </spine>
</package>"#
    )
}

// ---------------------------------------------------------------------------
// Malformed (overlapping) XHTML is rejected cleanly
// ---------------------------------------------------------------------------

#[test]
fn malformed_xhtml_is_rejected_with_a_clean_error() {
    let body = "<p><b>overlapping</em> markup</b></p>";
    let dir = tmp_dir("malformed");
    let input = dir.join("in.epub");
    write_archive(
        &input,
        &[
            ("META-INF/container.xml", utf8(CONTAINER)),
            ("OEBPS/content.opf", utf8(&opf_single("broken"))),
            ("OEBPS/ch1.xhtml", utf8(&chapter("Broken", body))),
        ],
    );

    let message = expect_read_error(&input);
    assert!(!message.is_empty());
}

// ---------------------------------------------------------------------------
// Truncation at every byte boundary of a small sample chapter
// ---------------------------------------------------------------------------

#[test]
fn truncated_chapter_survives_every_byte_boundary() {
    const SAMPLE_BODY: &str = "<h1>Alpha Beta</h1><p>Gamma <em>delta</em> epsilon.</p>\
<ul><li>Zeta.</li></ul><table><tr><td>Eta.</td></tr></table>";
    let document = chapter("Truncation", SAMPLE_BODY);
    let bytes = document.as_bytes();

    let dir = tmp_dir("truncated");
    let epub = dir.join("truncated.epub");
    let mut parsed = 0usize;
    for cut in 0..=bytes.len() {
        write_archive(
            &epub,
            &[
                ("META-INF/container.xml", utf8(CONTAINER)),
                ("OEBPS/content.opf", utf8(&opf_single("truncate"))),
                ("OEBPS/ch1.xhtml", bytes[..cut].to_vec()),
            ],
        );
        // Both outcomes are graceful: parse what survives, or fail with a
        // clean error. Neither may panic.
        if let Ok(book) = read_epub(&epub) {
            assert!(!book.blocks.is_empty());
            parsed += 1;
        }
    }
    assert!(
        parsed > 0,
        "at least some truncation points must still yield a usable book"
    );
}

// ---------------------------------------------------------------------------
// Duplicate manifest ids: first definition wins, spine still resolves
// ---------------------------------------------------------------------------

#[test]
fn duplicate_manifest_ids_keep_first_definition_in_full_read() {
    let opf = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" unique-identifier="uid" version="3.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="uid">dup-fixture</dc:identifier>
    <dc:title>Dup Fixture</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="dup" href="first.xhtml" media-type="application/xhtml+xml"/>
    <item id="other" href="second.xhtml" media-type="application/xhtml+xml"/>
    <item id="dup" href="second.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="dup"/>
    <itemref idref="other"/>
  </spine>
</package>"#;
    let dir = tmp_dir("dup-ids");
    let input = dir.join("in.epub");
    write_archive(
        &input,
        &[
            ("META-INF/container.xml", utf8(CONTAINER)),
            ("OEBPS/content.opf", utf8(opf)),
            (
                "OEBPS/first.xhtml",
                utf8(&chapter("Dup", "<p>firstword lands here.</p>")),
            ),
            (
                "OEBPS/second.xhtml",
                utf8(&chapter("Dup", "<p>secondword lands there.</p>")),
            ),
        ],
    );

    let book = read_epub(&input).expect("duplicate ids must resolve deterministically");
    assert_eq!(
        book.manifest.iter().filter(|item| item.id == "dup").count(),
        1,
        "duplicate ids collapse to the first definition"
    );
    let hrefs = book
        .sections
        .iter()
        .map(|section| section.href.as_str())
        .collect::<Vec<_>>();
    assert!(
        hrefs.iter().any(|href| href.ends_with("first.xhtml")),
        "spine idref dup must resolve to the first href: {hrefs:?}"
    );
    assert!(book.blocks.iter().any(|b| marked(b).contains("firstword")));
}

// ---------------------------------------------------------------------------
// Lying OPF paths fail with clean, attributable errors
// ---------------------------------------------------------------------------

#[test]
fn lying_opf_paths_fail_cleanly() {
    // Container points at a package that does not exist.
    let dir = tmp_dir("lying-opf");
    let ghost = dir.join("ghost.epub");
    write_archive(
        &ghost,
        &[
            (
                "META-INF/container.xml",
                utf8(&container_with("OEBPS/ghost.opf")),
            ),
            ("OEBPS/content.opf", utf8(OPF_MINIMAL)),
        ],
    );
    let message = expect_read_error(&ghost);
    assert!(message.contains("ghost.opf"), "unexpected error: {message}");

    // Manifest href lies about the chapter's presence in the archive.
    let missing = dir.join("missing-chapter.epub");
    let opf_missing = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" unique-identifier="uid" version="3.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="uid">lying</dc:identifier>
    <dc:title>Lying</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="ch1" href="absent-chapter.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine><itemref idref="ch1"/></spine>
</package>"#;
    write_archive(
        &missing,
        &[
            ("META-INF/container.xml", utf8(CONTAINER)),
            ("OEBPS/content.opf", utf8(opf_missing)),
        ],
    );
    let message = expect_read_error(&missing);
    assert!(
        message.contains("absent-chapter.xhtml"),
        "unexpected error: {message}"
    );

    // Spine references a manifest id that was never declared.
    let dangling = dir.join("dangling-idref.epub");
    let opf_dangling = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" unique-identifier="uid" version="3.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="uid">dangling</dc:identifier>
    <dc:title>Dangling</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="real" href="real.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine><itemref idref="ghostid"/></spine>
</package>"#;
    write_archive(
        &dangling,
        &[
            ("META-INF/container.xml", utf8(CONTAINER)),
            ("OEBPS/content.opf", utf8(opf_dangling)),
            (
                "OEBPS/real.xhtml",
                utf8(&chapter("Dangling", "<p>present.</p>")),
            ),
        ],
    );
    let message = expect_read_error(&dangling);
    assert!(
        message.contains("missing manifest id 'ghostid'"),
        "unexpected error: {message}"
    );
}

// ---------------------------------------------------------------------------
// EPUB3 nav without `properties="nav"`: suffix backstop keeps parse safe
// ---------------------------------------------------------------------------

#[test]
fn nav_without_nav_property_is_detected_and_parsed_safely() {
    let opf = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" unique-identifier="uid" version="3.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="uid">navless-prop</dc:identifier>
    <dc:title>Nav Property Missing</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="ch1" href="ch1.xhtml" media-type="application/xhtml+xml"/>
    <item id="navdoc" href="nav.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine><itemref idref="ch1"/></spine>
</package>"#;
    const NAV_DOC: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<head><title>Contents</title></head>
<body><nav epub:type="toc"><ol><li><a href="ch1.xhtml">Backstop Label</a></li></ol></nav></body>
</html>"#;

    let dir = tmp_dir("nav-backstop");
    let input = dir.join("in.epub");
    write_archive(
        &input,
        &[
            ("META-INF/container.xml", utf8(CONTAINER)),
            ("OEBPS/content.opf", utf8(opf)),
            (
                "OEBPS/ch1.xhtml",
                utf8(&chapter("Navless", "<p>body payload.</p>")),
            ),
            ("OEBPS/nav.xhtml", utf8(NAV_DOC)),
        ],
    );

    let inspection = inspect_epub(&input).expect("inspection must survive");
    assert!(
        inspection.has_nav,
        "nav.xhtml href backstop must detect the nav document"
    );

    let book = read_epub(&input).expect("parse must remain safe");
    let nav_section = book
        .sections
        .iter()
        .find(|section| section.title.as_deref() == Some("EPUB navigation"))
        .expect("nav labels must be extracted into a synthetic section");
    let nav_block_texts = nav_section
        .block_ids
        .iter()
        .filter_map(|id| book.blocks.iter().find(|block| &block.id == id))
        .map(marked)
        .collect::<Vec<_>>();
    assert!(
        nav_block_texts
            .iter()
            .any(|text| text.contains("Backstop Label")),
        "nav label text must be captured: {nav_block_texts:?}"
    );
}

// ---------------------------------------------------------------------------
// Percent-encoded and odd hrefs resolve against real archive names
// ---------------------------------------------------------------------------

#[test]
fn percent_encoded_hrefs_resolve_to_real_entries() {
    let opf = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" unique-identifier="uid" version="3.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="uid">hrefs</dc:identifier>
    <dc:title>Odd Hrefs</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="a" href="Text/Chapter%201.xhtml" media-type="application/xhtml+xml"/>
    <item id="b" href="weird%zzname.xhtml" media-type="application/xhtml+xml"/>
    <item id="c" href="%E2%82%AC.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="a"/>
    <itemref idref="b"/>
    <itemref idref="c"/>
  </spine>
</package>"#;

    let dir = tmp_dir("odd-hrefs");
    let input = dir.join("in.epub");
    write_archive(
        &input,
        &[
            ("META-INF/container.xml", utf8(CONTAINER)),
            ("OEBPS/content.opf", utf8(opf)),
            (
                "OEBPS/Text/Chapter 1.xhtml",
                utf8(&chapter("Hrefs", "<p>spaceword one.</p>")),
            ),
            (
                "OEBPS/weird%zzname.xhtml",
                utf8(&chapter("Hrefs", "<p>percentword two.</p>")),
            ),
            (
                "OEBPS/\u{20ac}.xhtml",
                utf8(&chapter("Hrefs", "<p>eurochar three.</p>")),
            ),
        ],
    );

    let book = read_epub(&input).expect("decoded hrefs must reach the real entries");
    for sentinel in ["spaceword", "percentword", "eurochar"] {
        assert!(
            book.blocks
                .iter()
                .any(|block| marked(block).contains(sentinel)),
            "chapter behind odd href must be captured: {sentinel}"
        );
    }
}

// ---------------------------------------------------------------------------
// Mixed-case extensions flow through read, coverage, and rebuild
// ---------------------------------------------------------------------------

#[test]
fn mixed_case_extensions_flow_through_the_pipeline() {
    const UPPER_OPF: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" unique-identifier="uid" version="3.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="uid">uppercase</dc:identifier>
    <dc:title>Mixed Case</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="ch1" href="CHAPTER.XHTML" media-type="application/xhtml+xml"/>
    <item id="alt" href="ALT.HtM" media-type="application/xhtml+xml"/>
    <item id="notes" href="meta.TXT" media-type="text/plain"/>
  </manifest>
  <spine>
    <itemref idref="ch1"/>
    <itemref idref="alt"/>
  </spine>
</package>"#;

    let dir = tmp_dir("mixed-case");
    let input = dir.join("in.epub");
    write_archive(
        &input,
        &[
            (
                "META-INF/container.xml",
                utf8(&container_with("OEBPS/content.OpF")),
            ),
            ("OEBPS/content.OpF", utf8(UPPER_OPF)),
            (
                "OEBPS/CHAPTER.XHTML",
                utf8(&chapter("Upper", "<p>upper headline.</p>")),
            ),
            (
                "OEBPS/ALT.HtM",
                utf8(&chapter("Upper", "<p>altern headline.</p>")),
            ),
            ("meta.TXT", b"plain notes file".to_vec()),
        ],
    );

    let book = read_epub(&input).expect("mixed-case extensions must parse");
    let chapter_blocks = book.blocks.clone();
    assert!(!chapter_blocks.is_empty());
    assert_eq!(
        book.spine.len(),
        2,
        "both spine entries must be registered regardless of extension case"
    );

    let translations = chapter_blocks
        .iter()
        .map(|block| BlockTranslation {
            block_id: block.id.clone(),
            text: format!("[Tr]{}", marked(block)),
        })
        .collect::<Vec<_>>();
    let output = dir.join("out.epub");
    rebuild_epub_with_options(
        &book,
        &translations,
        &output,
        &RebuildOptions {
            target_language: Some("Italian".to_string()),
            mode: BilingualMode::Replace,
            ..RebuildOptions::default()
        },
    )
    .expect("language rebuild over uppercase extensions must succeed");

    let rebuilt_chapter = entry_text(&output, "OEBPS/CHAPTER.XHTML");
    assert!(is_well_formed(&rebuilt_chapter));
    assert!(
        rebuilt_chapter.contains(r#"lang="it""#) && rebuilt_chapter.contains("[Tr]"),
        "uppercase-xhtml chapter must be language-patched and translated"
    );
    // The non-XHTML resource ships through unchanged.
    assert_eq!(
        entry_bytes(&output, "meta.TXT"),
        b"plain notes file".to_vec(),
        "non-XHTML resources are copied verbatim"
    );
}

// ---------------------------------------------------------------------------
// EPUB2 / OEBPS package: parse succeeds with honest coverage (no nav)
// ---------------------------------------------------------------------------

const NCX_V2: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
  <head/>
  <docTitle><text>EPUB2 Fixture</text></docTitle>
  <navMap>
    <navPoint id="np1" playOrder="1">
      <navLabel><text>Chapter One</text></navLabel>
      <content src="ch1.xhtml"/>
    </navPoint>
  </navMap>
</ncx>"#;

fn opf_epub2() -> String {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" xmlns:dc="http://purl.org/dc/elements/1.1/" unique-identifier="uid" version="2.0">
  <metadata>
    <dc:identifier id="uid">epub2-fixture</dc:identifier>
    <dc:title>EPUB2 Fixture</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
    <item id="ch1" href="ch1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine toc="ncx">
    <itemref idref="ch1"/>
  </spine>
  <guide>
    <reference type="text" title="Begin Reading" href="ch1.xhtml#start"/>
  </guide>
</package>"#
        .to_string()
}

#[test]
fn epub2_package_parses_with_honest_coverage_without_nav() {
    const BODY: &str = "<h1>Omega</h1><p>Alpha beta.</p>";
    let dir = tmp_dir("epub2");
    let input = dir.join("in.epub");
    write_archive(
        &input,
        &[
            ("META-INF/container.xml", utf8(CONTAINER)),
            ("OEBPS/content.opf", utf8(&opf_epub2())),
            ("OEBPS/toc.ncx", utf8(NCX_V2)),
            ("OEBPS/ch1.xhtml", utf8(&chapter("EPUB2 Fixture", BODY))),
        ],
    );

    // Sensible degradation decision pinned here: an EPUB2 book parses fine;
    // there is simply no nav document, and coverage math does not assume one.
    let inspection = inspect_epub(&input).expect("EPUB2 must inspect");
    assert!(
        !inspection.has_nav,
        "no nav doc exists in this EPUB2 fixture"
    );
    assert!(inspection.has_toc, "the NCX toc must still be detected");
    assert_eq!(inspection.title.as_deref(), Some("EPUB2 Fixture"));

    let book = read_epub(&input).expect("EPUB2 must parse into IR");
    assert!(
        !book
            .sections
            .iter()
            .any(|section| section.title.as_deref() == Some("EPUB navigation")),
        "an EPUB2 book must not grow a nav section"
    );
    // NCX labels are captured honestly as their own synthetic section.
    let toc_texts = book
        .sections
        .iter()
        .find(|section| section.href.ends_with("toc.ncx"))
        .map(|section| {
            section
                .block_ids
                .iter()
                .filter_map(|id| book.blocks.iter().find(|block| &block.id == id))
                .map(marked)
                .collect::<Vec<_>>()
        })
        .expect("NCX synthetic section exists");
    assert!(
        toc_texts.iter().any(|text| text.contains("Chapter One")),
        "NCX labels must be captured: {toc_texts:?}"
    );

    // Coverage is honest: the fully-captured controlled body reports 100%,
    // computed without any nav assumption. The EPUB3 twin below proves the
    // metric is independent of nav presence.
    let coverage_v2 = text_coverage(&input).expect("coverage should compute");
    assert_eq!(
        coverage_v2.captured_chars, coverage_v2.total_chars,
        "fully captured EPUB2 body must report full coverage"
    );
    assert_eq!(coverage_v2.percent(), 100.0);

    // Rebuild works on EPUB2 packages too. Only the spine chapter's blocks
    // are translated here (OPF metadata and NCX live in other entries).
    let ch1_section_ids = book
        .sections
        .iter()
        .filter(|section| section.href.ends_with("ch1.xhtml"))
        .map(|section| section.id.clone())
        .collect::<std::collections::HashSet<_>>();
    let translations = book
        .blocks
        .clone()
        .into_iter()
        .filter(|block| ch1_section_ids.contains(&block.section_id))
        .map(|block| BlockTranslation {
            block_id: block.id.clone(),
            text: format!("[Tr]{}", marked(&block)),
        })
        .collect::<Vec<_>>();
    let output = dir.join("out.epub");
    rebuild_epub_with_options(
        &book,
        &translations,
        &output,
        &RebuildOptions {
            mode: BilingualMode::Replace,
            ..RebuildOptions::default()
        },
    )
    .expect("EPUB2 rebuild must succeed");
    let rebuilt = entry_text(&output, "OEBPS/ch1.xhtml");
    assert_eq!(
        rebuilt.matches("[Tr]").count(),
        translations.len(),
        "every extracted block applies under replace"
    );
}

#[test]
fn epub3_twin_has_identical_coverage_metric_independent_of_nav() {
    const BODY: &str = "<h1>Omega</h1><p>Alpha beta.</p>";
    let opf = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" unique-identifier="uid" version="3.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="uid">epub3-twin</dc:identifier>
    <dc:title>EPUB2 Fixture</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="ch1" href="ch1.xhtml" media-type="application/xhtml+xml"/>
    <item id="navdoc" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
  </manifest>
  <spine><itemref idref="ch1"/></spine>
</package>"#;
    const NAV_DOC: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<head><title>Contents</title></head>
<body><nav epub:type="toc"><ol><li><a href="ch1.xhtml">Twin</a></li></ol></nav></body>
</html>"#;

    let dir = tmp_dir("epub3-twin");
    let input = dir.join("twin.epub");
    write_archive(
        &input,
        &[
            ("META-INF/container.xml", utf8(CONTAINER)),
            ("OEBPS/content.opf", utf8(opf)),
            ("OEBPS/nav.xhtml", utf8(NAV_DOC)),
            ("OEBPS/ch1.xhtml", utf8(&chapter("EPUB2 Fixture", BODY))),
        ],
    );

    let inspection = inspect_epub(&input).expect("twin must inspect");
    assert!(inspection.has_nav);
    let coverage_v3 = text_coverage(&input).expect("twin coverage computes");
    assert_eq!(coverage_v3.percent(), 100.0);
    // Coverage counts spine documents only; adding a nav document must not
    // move the denominator or numerator for identical chapter bodies.
    assert_eq!(
        coverage_v3.captured_chars,
        b"EPUB2Fixture".len() + b"Omega".len() + b"Alphabeta.".len(),
        "captured == title + heading + paragraph non-whitespace chars"
    );
}
