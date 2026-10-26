//! Dashboard audiobook coverage: estimates, cancellation, and artifact
//! publication. Fixtures are synthetic and every provider stays offline.

use super::*;
use audiobook::AudioProviderKind;
use std::io::Write as _;

const HEADING_TAG_SUFFIX: &str = " <break time=\"0.6s\" />";

fn chapter_xhtml(heading: &str, paragraph: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<head></head>
<body>
<h1>{heading}</h1>
<p>{paragraph}</p>
</body>
</html>"#
    )
}

/// Two chapters, each opening with an `h1` that becomes its title chunk.
fn build_headed_fixture_epub(path: &Path) {
    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    let mut zip = ZipWriter::new(std::fs::File::create(path).unwrap());
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    zip.start_file("mimetype", stored).unwrap();
    zip.write_all(b"application/epub+zip").unwrap();
    zip.start_file("META-INF/container.xml", stored).unwrap();
    zip.write_all(FIXTURE_CONTAINER_XML.as_bytes()).unwrap();
    zip.start_file("content.opf", stored).unwrap();
    zip.write_all(FIXTURE_OPF.as_bytes()).unwrap();
    zip.start_file("chapter1.xhtml", stored).unwrap();
    zip.write_all(
        chapter_xhtml(
            "Opening Notes",
            "A gray heron waited beside the canal while the ferry crossed.",
        )
        .as_bytes(),
    )
    .unwrap();
    zip.start_file("chapter2.xhtml", stored).unwrap();
    zip.write_all(
        chapter_xhtml(
            "Closing Notes",
            "By evening the market stalls were folded and the square was quiet.",
        )
        .as_bytes(),
    )
    .unwrap();
    zip.finish().unwrap();
}

#[test]
fn audiobook_estimate_counts_model_heading_breaks_and_chapter_filter() {
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("book.epub");
    build_headed_fixture_epub(&input);
    let estimate = |model: &str, filter: Option<std::collections::BTreeSet<usize>>| {
        plan_audiobook_estimate(
            &input,
            temp.path(),
            AudioProviderKind::Elevenlabs,
            model,
            2_000,
            filter,
        )
        .unwrap()
    };

    // eleven_v3 never receives break tags; Flash v2.5 does under `auto`.
    let plain = estimate("eleven_v3", None);
    let tagged = estimate("eleven_flash_v2_5", None);
    assert_eq!(plain.0, 2, "{plain:?}");
    assert_eq!((tagged.0, tagged.1), (plain.0, plain.1));
    assert_eq!(
        tagged.2 - plain.2,
        2 * HEADING_TAG_SUFFIX.chars().count(),
        "each chapter title carries exactly one break tag"
    );

    // An Auto estimate has no resolved model yet, so it carries no tags.
    assert_eq!(estimate("", None), plain);

    let filtered = estimate("eleven_flash_v2_5", Some([2].into()));
    assert_eq!(filtered.0, 1);
    assert!(filtered.1 < tagged.1);
    assert!(filtered.2 < tagged.2);
}

#[test]
fn audiobook_estimate_rejects_heading_break_without_text_room() {
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("book.epub");
    build_headed_fixture_epub(&input);
    // The break tag plus its separating space is 22 scalars: a 22-character
    // budget leaves no room for any heading text.
    let error = plan_audiobook_estimate(
        &input,
        temp.path(),
        AudioProviderKind::Elevenlabs,
        "eleven_flash_v2_5",
        22,
        None,
    )
    .unwrap_err();
    assert!(error.to_string().contains("heading break tag"), "{error:#}");
    // A model without break tags can use the same small budget.
    plan_audiobook_estimate(
        &input,
        temp.path(),
        AudioProviderKind::Elevenlabs,
        "eleven_v3",
        22,
        None,
    )
    .unwrap();
}

#[test]
fn audiobook_estimate_rejects_a_book_without_narratable_text() {
    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("empty.epub");
    let mut zip = ZipWriter::new(std::fs::File::create(&input).unwrap());
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    zip.start_file("mimetype", stored).unwrap();
    zip.write_all(b"application/epub+zip").unwrap();
    zip.start_file("META-INF/container.xml", stored).unwrap();
    zip.write_all(FIXTURE_CONTAINER_XML.as_bytes()).unwrap();
    zip.start_file("content.opf", stored).unwrap();
    zip.write_all(FIXTURE_OPF.as_bytes()).unwrap();
    for name in ["chapter1.xhtml", "chapter2.xhtml"] {
        zip.start_file(name, stored).unwrap();
        zip.write_all(
            br#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head></head><body></body></html>"#,
        )
        .unwrap();
    }
    zip.finish().unwrap();

    let error = plan_audiobook_estimate(
        &input,
        temp.path(),
        AudioProviderKind::Mock,
        "mock-silence",
        2_000,
        None,
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("no narratable text"),
        "{error:#}"
    );
}

#[test]
fn audiobook_quota_fits_uses_estimated_credits_with_rounding() {
    let flash =
        crate::audio_cost::estimate_audio_cost("elevenlabs", "eleven_flash_v2_5", 5_000).unwrap();
    let credits = flash
        .credits
        .expect("the bundled table prices Flash in credits");
    assert!(
        credits < 5_000.0,
        "Flash bills below one credit per character"
    );
    let needed = credits.ceil() as u64;
    // Credits, not characters, decide the fit when a price exists.
    assert!(audiobook::elevenlabs_quota_fits(
        5_000,
        Some(credits),
        needed
    ));
    assert!(!audiobook::elevenlabs_quota_fits(
        5_000,
        Some(credits),
        needed - 1
    ));
    // Fractional credits round up before comparison.
    assert!(audiobook::elevenlabs_quota_fits(5_000, Some(700.0), 700));
    assert!(!audiobook::elevenlabs_quota_fits(5_000, Some(700.1), 700));
    // Without a pricing entry, characters are the conservative stand-in.
    assert!(!audiobook::elevenlabs_quota_fits(5_000, None, 4_999));
    assert!(audiobook::elevenlabs_quota_fits(5_000, None, 5_000));
}

/// A pdftohtml-shaped EPUB: one spine document per printed page, each
/// carrying the generator marker that turns on PDF cleanup and page grouping.
fn build_pdf_paged_fixture_epub(path: &Path, pages: usize) {
    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    let mut zip = ZipWriter::new(std::fs::File::create(path).unwrap());
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    zip.start_file("mimetype", stored).unwrap();
    zip.write_all(b"application/epub+zip").unwrap();
    zip.start_file("META-INF/container.xml", stored).unwrap();
    zip.write_all(FIXTURE_CONTAINER_XML.as_bytes()).unwrap();
    let mut manifest = String::new();
    let mut spine = String::new();
    for page in 1..=pages {
        let html = format!(
            r#"<html xmlns="http://www.w3.org/1999/xhtml"><head><title>Printed page</title><meta name="generator" content="pdftohtml"/></head><body><p>Sheet {page} describes a lighthouse keeper counting gulls along the breakwater.</p></body></html>"#
        );
        zip.start_file(format!("page{page}.xhtml"), stored).unwrap();
        zip.write_all(html.as_bytes()).unwrap();
        manifest.push_str(&format!(
            r#"<item id="p{page}" href="page{page}.xhtml" media-type="application/xhtml+xml"/>"#
        ));
        spine.push_str(&format!(r#"<itemref idref="p{page}"/>"#));
    }
    let opf = format!(
        r#"<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="uid">pdf-paged-fixture</dc:identifier><dc:title>Paged Fixture</dc:title><dc:language>en</dc:language></metadata><manifest>{manifest}</manifest><spine>{spine}</spine></package>"#
    );
    zip.start_file("content.opf", stored).unwrap();
    zip.write_all(opf.as_bytes()).unwrap();
    zip.finish().unwrap();
}

/// The estimate endpoint must group PDF pages exactly like narration does,
/// instead of quoting one chapter per printed page.
#[tokio::test]
async fn audiobook_estimate_groups_pdf_pages_like_narration() {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    const PAGES: usize = 8;
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("paged.epub");
    build_pdf_paged_fixture_epub(&input, PAGES);

    let scratch = tempfile::tempdir().unwrap();
    let narration = bookforge_audio::read_narration_source(&input, scratch.path()).unwrap();
    assert!(
        narration.pdf_page_grouping,
        "the generator marker must enable page grouping"
    );
    let expected = plan_audiobook_estimate(
        &input,
        scratch.path(),
        AudioProviderKind::Mock,
        "mock-silence",
        2_000,
        None,
    )
    .unwrap();
    assert!(
        expected.0 < PAGES,
        "grouping must not quote one chapter per page: {expected:?}"
    );

    let mut body = b"--B\r\nContent-Disposition: form-data; name=\"provider\"\r\n\r\nmock\r\n--B\r\nContent-Disposition: form-data; name=\"file\"; filename=\"paged.epub\"\r\nContent-Type: application/epub+zip\r\n\r\n".to_vec();
    body.extend_from_slice(&std::fs::read(&input).unwrap());
    body.extend_from_slice(b"\r\n--B--\r\n");
    let upload_dir = tempfile::tempdir().unwrap();
    let response = dashboard_router(test_state_with_upload_dir(
        "token-123",
        upload_dir.path().to_path_buf(),
    ))
    .oneshot(
        Request::builder()
            .method("POST")
            .uri("/api/audiobook/estimate")
            .header("host", TEST_HOST)
            .header("cookie", session_cookie_value("token-123"))
            .header("content-type", "multipart/form-data; boundary=B")
            .body(Body::from(body))
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let estimate = response_json(response).await;
    assert_eq!(estimate["chapters"], json!(expected.0));
    assert_eq!(estimate["chunks"], json!(expected.1));
    assert_eq!(estimate["characters"], json!(expected.2));
}
