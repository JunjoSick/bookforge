use std::io::Read;
use std::path::Path;

use bookforge_core::segment::{BlockTranslation, Segment};

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
                        validate_xhtml_content(&mut report, name, &content, segments, block_translations);
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
}

fn validate_xhtml_content(
    report: &mut EpubValidationReport,
    href: &str,
    content: &str,
    segments: &[Segment],
    block_translations: &[BlockTranslation],
) {
    let by_block_id = block_translations
        .iter()
        .map(|bt| (bt.block_id.0.as_str(), bt.text.as_str()))
        .collect::<std::collections::HashMap<_, _>>();

    for segment in segments {
        for block in &segment.source.blocks {
            let translated = by_block_id.get(block.block_id.0.as_str()).copied().unwrap_or(&block.text);

            for marker in &segment.constraints.preserve_markers {
                if block.text.contains(marker) && !translated.contains(marker) {
                    report.issues.push(EpubValidationIssue {
                        severity: ValidationSeverity::Error,
                        kind: "missing_marker".to_string(),
                        href: Some(href.to_string()),
                        block_id: Some(block.block_id.0.clone()),
                        message: format!("Required marker '{marker}' missing in translation of block {}", block.block_id.0),
                    });
                }
            }

            for span in &block.protected_spans {
                if block.text.contains(span) && !translated.contains(span) {
                    report.issues.push(EpubValidationIssue {
                        severity: ValidationSeverity::Warning,
                        kind: "missing_protected_span".to_string(),
                        href: Some(href.to_string()),
                        block_id: Some(block.block_id.0.clone()),
                        message: format!("Protected span '{}' may be missing in block {}", span, block.block_id.0),
                    });
                }
            }
        }
    }

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
    let mut stack: Vec<String> = Vec::new();
    let chars: Vec<char> = content.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '<' && i + 1 < chars.len() {
            if chars[i + 1] == '/' {
                let close_end = chars[i..].iter().position(|&c| c == '>');
                if let Some(end) = close_end {
                    let tag: String = chars[i + 2..i + end].iter().collect();
                    let tag = tag.split_whitespace().next().unwrap_or("").trim().to_string();
                    if stack.is_empty() || stack.last() != Some(&tag) {
                        return true;
                    }
                    stack.pop();
                    i += end + 1;
                    continue;
                }
            } else if chars[i + 1] != '?' && chars[i + 1] != '!' {
                let end = chars[i..].iter().position(|&c| c == '>');
                if let Some(end_pos) = end {
                    let rest: String = chars[i..i + end_pos + 1].iter().collect();
                    if !rest.contains("/>") {
                        let tag: String = chars[i + 1..i + end_pos].iter().collect();
                        let tag = tag.split_whitespace().next().unwrap_or("").trim().to_string();
                        if !tag.is_empty() {
                            stack.push(tag);
                        }
                    }
                    i += end_pos + 1;
                    continue;
                }
            }
        }
        i += 1;
    }
    !stack.is_empty()
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
            let translated = by_block_id.get(block.block_id.0.as_str()).copied().unwrap_or(&block.text);

            if block.text.is_empty() && translated.is_empty() {
                continue;
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
    fn detects_broken_xml_nesting() {
        assert!(has_broken_xml("<p><b>text</p></b>"));
    }

    #[test]
    fn accepts_well_formed_xml() {
        assert!(!has_broken_xml("<p>Hello <b>world</b></p>"));
    }

    #[test]
    fn detects_empty_translation() {
        use bookforge_core::segment::{
            Segment, SegmentBlock, SegmentConstraints, SegmentContext, SegmentId, SegmentMetadata,
            SegmentSource, SegmentTextRun,
        };
        use bookforge_core::ir::SectionId;

        let block_id = bookforge_core::ir::BlockId("b_01".to_string());
        let segment = Segment {
            id: SegmentId("seg_01".to_string()),
            section_id: SectionId("sec_01".to_string()),
            ordinal: 0,
            block_ids: vec![block_id.clone()],
            source: SegmentSource {
                text: "Hello".to_string(),
                blocks: vec![SegmentBlock {
                    block_id: block_id.clone(),
                    kind: "p".to_string(),
                    text: "Hello".to_string(),
                    text_runs: vec![SegmentTextRun {
                        id: "r0".to_string(),
                        text: "Hello".to_string(),
                    }],
                    protected_spans: vec![],
                }],
                token_estimate: 1,
            },
            context: SegmentContext::default(),
            metadata: SegmentMetadata::default(),
            constraints: SegmentConstraints::default(),
            checksum: "abc".to_string(),
        };
        let issues = validate_block_translations(
            &[segment],
            &[BlockTranslation {
                block_id: block_id.clone(),
                text: String::new(),
            }],
        );
        assert!(!issues.is_empty());
    }
}
