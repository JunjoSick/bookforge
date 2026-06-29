use std::io::Read;
use std::path::Path;

use bookforge_core::{
    marker::marker_ids_in_text,
    segment::{BlockTranslation, Segment},
};

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
            let mut file_names = Vec::new();
            for i in 0..archive.len() {
                if let Ok(entry) = archive.by_index(i) {
                    file_names.push(entry.name().to_string());
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

            for name in &file_names {
                if (name.ends_with(".xhtml") || name.ends_with(".html") || name.ends_with(".opf"))
                    && let Ok(mut entry) = archive.by_name(name)
                {
                    let mut content = String::new();
                    if entry.read_to_string(&mut content).is_ok() {
                        validate_xhtml_content(&mut report, name, &content);
                    }
                }
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
                if block.text.contains(span) && !translated.contains(span) {
                    issues.push(EpubValidationIssue {
                        severity: ValidationSeverity::Warning,
                        kind: "missing_protected_span".to_string(),
                        href: None,
                        block_id: Some(block.block_id.0.clone()),
                        message: format!(
                            "Protected span '{}' may be missing in block {}",
                            span, block.block_id.0
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

    fn segment_with_block(
        block_text: &str,
        protected_spans: Vec<String>,
    ) -> (Segment, bookforge_core::ir::BlockId) {
        use bookforge_core::ir::{BlockId, SectionId};
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
                    protected_spans,
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
