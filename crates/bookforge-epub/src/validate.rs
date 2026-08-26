use std::path::Path;

use bookforge_core::{
    marker::marker_ids_in_text,
    segment::{BlockTranslation, Segment},
};

use crate::archive_limits::{DEFAULT_ARCHIVE_LIMITS, validate_archive_metadata};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpubValidationReport {
    pub xml_valid: bool,
    pub files_checked: usize,
    pub issues: Vec<EpubValidationIssue>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpubValidationIssue {
    pub severity: ValidationSeverity,
    pub kind: String,
    pub href: Option<String>,
    pub block_id: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationSeverity {
    Info,
    Warning,
    Error,
}

pub fn validate_translated_epub(
    epub_path: &Path,
    segments: &[Segment],
    block_translations: &[BlockTranslation],
) -> EpubValidationReport {
    let mut report = EpubValidationReport {
        xml_valid: true,
        files_checked: 0,
        issues: Vec::new(),
    };

    let Ok(file) = std::fs::File::open(epub_path) else {
        report.issues.push(EpubValidationIssue {
            severity: ValidationSeverity::Error,
            kind: "epub_missing".to_string(),
            href: None,
            block_id: None,
            message: format!("EPUB file not found: {}", epub_path.display()),
        });
        report.xml_valid = false;
        return report;
    };

    match zip::ZipArchive::new(file) {
        Ok(mut archive) => {
            // Both the archive's declared metadata and every entry read pass
            // through the shared decompression budget: `bookforge validate`
            // sees the same lying-small-entry defense as reflow/reader.
            let mut file_names = Vec::new();
            match validate_archive_metadata(&mut archive, DEFAULT_ARCHIVE_LIMITS) {
                Ok(mut read_budget) => {
                    // One indexed pass names every entry and validates the
                    // translatable ones; no per-name second scan and no
                    // decompression of entries whose contents we ignore.
                    for index in 0..archive.len() {
                        let Ok(mut entry) = archive.by_index(index) else {
                            continue;
                        };
                        let name = entry.name().to_string();
                        if !is_validatable_resource_name(&name) {
                            file_names.push(name);
                            continue;
                        }

                        let compressed_size = entry.compressed_size();
                        match read_budget.read_entry(&mut entry, &name, compressed_size) {
                            Ok(bytes) => {
                                if let Ok(content) = String::from_utf8(bytes) {
                                    validate_xhtml_content(&mut report, &name, &content);
                                }
                            }
                            Err(error) => {
                                report.xml_valid = false;
                                report.issues.push(EpubValidationIssue {
                                    severity: ValidationSeverity::Error,
                                    kind: "decompression_limit".to_string(),
                                    href: Some(name.clone()),
                                    block_id: None,
                                    message: error.to_string(),
                                });
                            }
                        }
                        file_names.push(name);
                    }
                }
                Err(error) => {
                    report.xml_valid = false;
                    report.issues.push(EpubValidationIssue {
                        severity: ValidationSeverity::Error,
                        kind: "decompression_limit".to_string(),
                        href: None,
                        block_id: None,
                        message: error.to_string(),
                    });
                }
            }
            report.files_checked = file_names.len();

            if !file_names.contains(&"mimetype".to_string()) {
                report.issues.push(EpubValidationIssue {
                    severity: ValidationSeverity::Error,
                    kind: "missing_mimetype".to_string(),
                    href: None,
                    block_id: None,
                    message: "EPUB missing mimetype file".to_string(),
                });
                report.xml_valid = false;
            }

            if !file_names.contains(&"META-INF/container.xml".to_string()) {
                report.issues.push(EpubValidationIssue {
                    severity: ValidationSeverity::Error,
                    kind: "missing_container".to_string(),
                    href: None,
                    block_id: None,
                    message: "EPUB missing META-INF/container.xml".to_string(),
                });
                report.xml_valid = false;
            }
        }
        Err(e) => {
            report.issues.push(EpubValidationIssue {
                severity: ValidationSeverity::Error,
                kind: "zip_open_failed".to_string(),
                href: None,
                block_id: None,
                message: format!("Failed to open EPUB as ZIP: {e}"),
            });
            report.xml_valid = false;
        }
    }

    report
        .issues
        .extend(validate_block_translations(segments, block_translations));

    report
}

fn validate_xhtml_content(report: &mut EpubValidationReport, href: &str, content: &str) {
    if has_broken_xml(content) {
        report.issues.push(EpubValidationIssue {
            severity: ValidationSeverity::Error,
            kind: "malformed_xhtml".to_string(),
            href: Some(href.to_string()),
            block_id: None,
            message: format!("Malformed XHTML in {href}"),
        });
        report.xml_valid = false;
    }
}

/// Entries whose well-formedness matters: document resources and the
/// package manifest. Extension matching is case-insensitive — untrusted
/// archives routinely ship `CHAPTER.XHTML` — but no second archive scan is
/// performed for the check.
fn is_validatable_resource_name(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(_, extension)| {
        matches!(
            extension.to_ascii_lowercase().as_str(),
            "xhtml" | "html" | "opf"
        )
    })
}

fn has_broken_xml(content: &str) -> bool {
    use quick_xml::{Reader, events::Event};
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(false);
    loop {
        match reader.read_event() {
            Ok(Event::Eof) => return false,
            Ok(_) => continue,
            Err(_) => return true,
        }
    }
}

pub fn validate_block_translations(
    segments: &[Segment],
    block_translations: &[BlockTranslation],
) -> Vec<EpubValidationIssue> {
    let mut issues = Vec::new();
    let by_block_id = block_translations
        .iter()
        .map(|bt| (bt.block_id.0.as_str(), bt.text.as_str()))
        .collect::<std::collections::HashMap<_, _>>();

    for segment in segments {
        for block in &segment.source.blocks {
            let translated = by_block_id
                .get(block.block_id.0.as_str())
                .copied()
                .unwrap_or(&block.text);

            if block.text.is_empty() && translated.is_empty() {
                continue;
            }

            let translated_markers = marker_ids_in_text(translated);
            for marker in marker_ids_in_text(&block.text) {
                if !translated_markers.contains(&marker) {
                    issues.push(EpubValidationIssue {
                        severity: ValidationSeverity::Error,
                        kind: "missing_marker".to_string(),
                        href: None,
                        block_id: Some(block.block_id.0.clone()),
                        message: format!(
                            "Required marker '{marker}' missing in translation of block {}",
                            block.block_id.0
                        ),
                    });
                }
            }

            for span in &block.protected_spans {
                if block.text.contains(&span.text) && !translated.contains(&span.text) {
                    issues.push(EpubValidationIssue {
                        severity: ValidationSeverity::Warning,
                        kind: "missing_protected_span".to_string(),
                        href: None,
                        block_id: Some(block.block_id.0.clone()),
                        message: format!(
                            "Protected span '{}' may be missing in block {}",
                            span.text, block.block_id.0
                        ),
                    });
                }
            }

            if translated.is_empty() && !block.text.is_empty() {
                issues.push(EpubValidationIssue {
                    severity: ValidationSeverity::Error,
                    kind: "empty_translation".to_string(),
                    href: None,
                    block_id: Some(block.block_id.0.clone()),
                    message: format!("Block {} has empty translation", block.block_id.0),
                });
            }

            let source_len = block.text.chars().count().max(1);
            let trans_len = translated.chars().count();
            let ratio = trans_len as f64 / source_len as f64;
            if !(0.1..=5.0).contains(&ratio) {
                issues.push(EpubValidationIssue {
                    severity: ValidationSeverity::Warning,
                    kind: "suspicious_length_ratio".to_string(),
                    href: None,
                    block_id: Some(block.block_id.0.clone()),
                    message: format!(
                        "Suspicious length ratio {:.2} for block {} (source={source_len}, translation={trans_len})",
                        ratio, block.block_id.0
                    ),
                });
            }
        }
    }

    issues
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validatable_resource_names_match_case_insensitively() {
        assert!(is_validatable_resource_name("chapter.xhtml"));
        assert!(is_validatable_resource_name("TEXT/CHAPTER.XHTML"));
        assert!(is_validatable_resource_name("index.Html"));
        assert!(is_validatable_resource_name("OEBPS/content.OPF"));
        assert!(!is_validatable_resource_name("toc.ncx"));
        assert!(!is_validatable_resource_name("images/cover.png"));
        assert!(!is_validatable_resource_name("META-INF/container.xml"));
    }

    #[test]
    fn validates_uppercase_extension_resources_and_reports_href() {
        use std::io::Write as _;
        use zip::{CompressionMethod, write::SimpleFileOptions};

        let fixture = temp_zip("bookforge-validate-uppercase", |mut writer| {
            let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
            let deflated =
                SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
            writer
                .start_file("mimetype", stored)
                .expect("mimetype should start");
            writer
                .write_all(b"application/epub+zip")
                .expect("mimetype should write");
            writer
                .start_file("META-INF/container.xml", deflated)
                .expect("container should start");
            writer.write_all(CONTAINER_XML.as_bytes()).expect("write");
            writer
                .start_file("content.opf", deflated)
                .expect("opf should start");
            writer
                .write_all(OPF_XML.as_bytes())
                .expect("opf should write");
            writer
                .start_file("CHAPTER.XHTML", deflated)
                .expect("chapter should start");
            writer
                .write_all(b"<html xmlns=\"http://www.w3.org/1999/xhtml\"><body><p><b>broken</p></b></body></html>")
                .expect("chapter should write");
            writer.finish().expect("fixture should finish");
        });
        let report = validate_translated_epub(&fixture, &[], &[]);
        let _ = std::fs::remove_file(&fixture);

        let malformed = report
            .issues
            .iter()
            .filter(|issue| issue.kind == "malformed_xhtml")
            .collect::<Vec<_>>();
        assert_eq!(malformed.len(), 1, "all issues: {:?}", report.issues);
        assert_eq!(malformed[0].href.as_deref(), Some("CHAPTER.XHTML"));
        assert!(!report.xml_valid);
        assert_eq!(report.files_checked, 4);
    }

    #[test]
    fn rejects_archives_over_the_entry_count_limit_as_decompression_limit() {
        use std::io::Write as _;
        use zip::{CompressionMethod, write::SimpleFileOptions};

        let fixture = temp_zip("bookforge-validate-entry-bomb", |mut writer| {
            let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
            writer
                .start_file("mimetype", stored)
                .expect("mimetype should start");
            writer
                .write_all(b"application/epub+zip")
                .expect("mimetype should write");
            for index in 0..10_001 {
                writer
                    .start_file(format!("blob{index}"), stored)
                    .expect("blob should start");
                writer.write_all(b"x").expect("blob should write");
            }
            writer.finish().expect("fixture should finish");
        });
        let report = validate_translated_epub(&fixture, &[], &[]);
        let _ = std::fs::remove_file(&fixture);

        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.kind == "decompression_limit"
                    && issue.message.contains("entry count limit exceeded")),
            "all issues: {:?}",
            report.issues
        );
        assert!(!report.xml_valid);
    }

    /// Minimal EPUB with one chapter whose body is injected verbatim, so
    /// XML-level problems are detectable by name.
    fn temp_zip<F>(label: &str, build: F) -> std::path::PathBuf
    where
        F: FnOnce(zip::ZipWriter<std::fs::File>),
    {
        use std::time::{SystemTime, UNIX_EPOCH};

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("{label}-{}-{nonce}.epub", std::process::id()));
        let file = std::fs::File::create(&path).expect("fixture should create");
        build(zip::ZipWriter::new(file));
        path
    }

    const CONTAINER_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;

    const OPF_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" unique-identifier="bookid" version="3.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="bookid">validate-fixture</dc:identifier>
    <dc:title>Validate Fixture</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="chapter" href="chapter.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="chapter"/>
  </spine>
</package>"#;

    fn segment_with_block(
        block_text: &str,
        protected_spans: Vec<String>,
    ) -> (Segment, bookforge_core::ir::BlockId) {
        use bookforge_core::ir::{BlockId, ProtectedSpan, ProtectedSpanKind, SectionId};
        use bookforge_core::segment::{
            SegmentBlock, SegmentConstraints, SegmentContext, SegmentId, SegmentMetadata,
            SegmentSource, SegmentTextRun,
        };

        let block_id = BlockId("b_01".to_string());
        let segment = Segment {
            id: SegmentId("seg_01".to_string()),
            section_id: SectionId("sec_01".to_string()),
            ordinal: 0,
            block_ids: vec![block_id.clone()],
            source: SegmentSource {
                text: block_text.to_string(),
                blocks: vec![SegmentBlock {
                    block_id: block_id.clone(),
                    kind: "p".to_string(),
                    text: block_text.to_string(),
                    text_runs: vec![SegmentTextRun {
                        id: "r0".to_string(),
                        text: block_text.to_string(),
                    }],
                    protected_spans: protected_spans
                        .into_iter()
                        .map(|text| ProtectedSpan {
                            kind: ProtectedSpanKind::Number,
                            text,
                        })
                        .collect(),
                }],
                token_estimate: 1,
            },
            context: SegmentContext::default(),
            metadata: SegmentMetadata::default(),
            constraints: SegmentConstraints::default(),
            checksum: "abc".to_string(),
        };

        (segment, block_id)
    }

    #[test]
    fn rejects_broken_xml() {
        assert!(has_broken_xml("<p><b>text</p></b>"));
    }

    #[test]
    fn accepts_well_formed_xml() {
        assert!(!has_broken_xml("<p>Hello <b>world</b></p>"));
    }

    #[test]
    fn accepts_self_closing_tags_with_attributes() {
        assert!(!has_broken_xml(
            r#"<root><img src="a.png" alt="x"/><br/></root>"#,
        ));
    }

    #[test]
    fn accepts_namespaced_xhtml() {
        let xhtml = r#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><p epub:type="chapter">x</p></body></html>"#;
        assert!(!has_broken_xml(xhtml));
    }

    #[test]
    fn accepts_comments_and_processing_instructions() {
        let xml = "<?xml version=\"1.0\"?><!-- a comment --><root><a/></root>";
        assert!(!has_broken_xml(xml));
    }

    #[test]
    fn detects_empty_translation() {
        let (segment, block_id) = segment_with_block("Hello", vec![]);
        let issues = validate_block_translations(
            &[segment],
            &[BlockTranslation {
                block_id: block_id.clone(),
                text: String::new(),
            }],
        );
        assert!(!issues.is_empty());
    }

    #[test]
    fn missing_marker_is_reported_once_without_epub_href() {
        let (segment, block_id) = segment_with_block("<m1>Hello</m1>", vec![]);
        let issues = validate_block_translations(
            &[segment],
            &[BlockTranslation {
                block_id: block_id.clone(),
                text: "Ciao".to_string(),
            }],
        );
        let marker_issues = issues
            .iter()
            .filter(|issue| issue.kind == "missing_marker")
            .collect::<Vec<_>>();

        assert_eq!(marker_issues.len(), 1);
        assert_eq!(marker_issues[0].href, None);
        assert_eq!(marker_issues[0].block_id.as_deref(), Some("b_01"));
    }

    #[test]
    fn missing_protected_span_is_reported_once_without_epub_href() {
        let (segment, block_id) = segment_with_block("Price 1,234", vec!["1,234".to_string()]);
        let issues = validate_block_translations(
            &[segment],
            &[BlockTranslation {
                block_id,
                text: "Prezzo".to_string(),
            }],
        );
        let span_issues = issues
            .iter()
            .filter(|issue| issue.kind == "missing_protected_span")
            .collect::<Vec<_>>();

        assert_eq!(span_issues.len(), 1);
        assert_eq!(span_issues[0].href, None);
        assert_eq!(span_issues[0].block_id.as_deref(), Some("b_01"));
    }
}
