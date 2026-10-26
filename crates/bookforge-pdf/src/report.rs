//! Conversion fidelity report. The contract from ROADMAP §9b: pages
//! that reconstruct badly are flagged, never hidden.

use serde::Serialize;

use crate::reconstruct::PageStats;

pub const LOW_CONFIDENCE_COVERAGE_RATIO: f64 = 0.95;
pub const LOW_CONFIDENCE_COVERAGE_PERCENT: f64 = LOW_CONFIDENCE_COVERAGE_RATIO * 100.0;

#[derive(Debug, Serialize)]
pub struct ConversionReport {
    pub input: String,
    pub output: String,
    pub pages: usize,
    pub blocks: usize,
    /// Non-whitespace characters in reconstructed blocks.
    pub reconstructed_chars: usize,
    /// Non-whitespace characters removed from text blocks because the same
    /// content was preserved in table/equation/figure raster crops.
    pub media_preserved_chars: usize,
    /// Non-whitespace characters in the raw `pdftotext` baseline.
    pub baseline_chars: usize,
    /// Non-whitespace characters deliberately removed as running heads and
    /// feet, folios and contents-page numbers: the sum of the pages'
    /// `running_header_chars`. The raw baseline still counts them.
    pub running_header_chars: usize,
    /// (reconstructed + media-preserved + removed running-margin
    /// characters) / baseline, capped at 100: the same credit the per-page
    /// confidence check gives (PDF-6). Above ~100 means the baseline missed
    /// text (rare); far below means reconstruction dropped content and the
    /// page list below says where. Meaningful only when
    /// `text_coverage_available` is true.
    pub coverage_percent: f64,
    /// Whether the PDF has any selectable text to measure coverage
    /// against. A scanned document has none, and its 100% means nothing.
    pub text_coverage_available: bool,
    pub two_column_pages: usize,
    /// Pages whose strong-script evidence leans Arabic/Hebrew-class
    /// (PDF-7). Observability only: RTL pages are judged by the same
    /// coverage ratio once invisible formatting characters are excluded
    /// on both sides.
    pub rtl_dominant_pages: usize,
    pub images: usize,
    pub figures: usize,
    pub tables: usize,
    pub equations: usize,
    pub low_confidence_pages: usize,
    pub ocr_pages: usize,
    pub page_stats: Vec<PageStats>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ReportMetrics {
    pub blocks: usize,
    pub reconstructed_chars: usize,
    pub media_preserved_chars: usize,
    pub baseline_chars: usize,
    pub images: usize,
    pub figures: usize,
    pub tables: usize,
    pub equations: usize,
    pub layout_warnings: Vec<String>,
}

impl ConversionReport {
    pub fn build(
        input: &str,
        output: &str,
        page_stats: Vec<PageStats>,
        metrics: ReportMetrics,
    ) -> Self {
        // Running heads, folios and contents-page numbers are removed on
        // purpose. The per-page threshold credits them back (PDF-6); the
        // document figure does the same, or a book with a running head on
        // every short page reported missing content it had dropped by design.
        let running_header_chars: usize = page_stats
            .iter()
            .map(|page| page.running_header_chars)
            .sum();
        let credited_chars = metrics
            .reconstructed_chars
            .saturating_add(metrics.media_preserved_chars)
            .saturating_add(running_header_chars);
        let coverage_percent = page_coverage_percent(credited_chars, metrics.baseline_chars);

        let mut warnings = Vec::new();
        if coverage_percent < LOW_CONFIDENCE_COVERAGE_PERCENT {
            warnings.push(format!(
                "reconstructed text covers only {coverage_percent:.1}% of the pdftotext baseline; some content was not captured"
            ));
        }
        for page in &page_stats {
            if page.image_only_candidate && page.low_confidence {
                let action = page.low_confidence_action.as_deref().unwrap_or("linearize");
                let detail = if action == "ocr" {
                    "OCR supplied its text, which no selectable-text baseline can check"
                } else {
                    "large image-only page needs OCR for translatable text"
                };
                warnings.push(format!(
                    "page {}: selectable text unavailable; {detail}; action={action}",
                    page.page
                ));
            } else if page.low_confidence {
                let action = page.low_confidence_action.as_deref().unwrap_or("linearize");
                // The figure the threshold judged, not the raw ratio.
                let page_coverage = page_coverage_percent(
                    page.chars.saturating_add(page.running_header_chars),
                    page.baseline_chars,
                );
                warnings.push(format!(
                    "page {}: low-confidence reconstruction ({page_coverage:.1}% of pdftotext baseline, {} reconstructed + {} removed as running heads and page numbers / {} baseline characters); action={action}",
                    page.page, page.chars, page.running_header_chars, page.baseline_chars
                ));
            } else if page.chars == 0 && page.baseline_chars > page.running_header_chars {
                // A page whose only text was its folio and running header
                // (a chapter-opening plate) lost nothing worth flagging.
                warnings.push(format!(
                    "page {}: no text reconstructed, but pdftotext found {} baseline characters",
                    page.page, page.baseline_chars
                ));
            }
        }
        warnings.extend(metrics.layout_warnings);

        Self {
            input: input.to_string(),
            output: output.to_string(),
            pages: page_stats.len(),
            blocks: metrics.blocks,
            reconstructed_chars: metrics.reconstructed_chars,
            media_preserved_chars: metrics.media_preserved_chars,
            baseline_chars: metrics.baseline_chars,
            running_header_chars,
            coverage_percent,
            text_coverage_available: metrics.baseline_chars > 0,
            two_column_pages: page_stats.iter().filter(|page| page.two_column).count(),
            rtl_dominant_pages: page_stats.iter().filter(|page| page.rtl_dominant).count(),
            images: metrics.images,
            figures: metrics.figures,
            tables: metrics.tables,
            equations: metrics.equations,
            low_confidence_pages: page_stats.iter().filter(|page| page.low_confidence).count(),
            ocr_pages: page_stats
                .iter()
                .filter(|page| page.low_confidence_action.as_deref() == Some("ocr"))
                .count(),
            page_stats,
            warnings,
        }
    }

    pub fn summary(&self) -> String {
        let coverage = if self.text_coverage_available {
            format!(
                "{:.1}% ({} reconstructed + {} preserved as images + {} removed as running heads and page numbers / {} baseline characters)",
                self.coverage_percent,
                self.reconstructed_chars,
                self.media_preserved_chars,
                self.running_header_chars,
                self.baseline_chars,
            )
        } else {
            "unavailable (no selectable-text baseline)".to_string()
        };
        let mut out = format!(
            "Pages: {}\nBlocks: {}\nTwo-column pages: {}\nRTL-dominant pages: {}\nImages: {} extracted, {} figure block(s)\nTables: {} crop(s)\nEquations: {} crop(s)\nLow-confidence pages: {}\nOCR-recovered pages: {}\nText coverage vs pdftotext: {coverage}\n",
            self.pages,
            self.blocks,
            self.two_column_pages,
            self.rtl_dominant_pages,
            self.images,
            self.figures,
            self.tables,
            self.equations,
            self.low_confidence_pages,
            self.ocr_pages,
        );
        if self.warnings.is_empty() {
            out.push_str("Warnings: none\n");
        } else {
            out.push_str("Warnings:\n");
            for warning in &self.warnings {
                out.push_str(&format!("  - {warning}\n"));
            }
        }
        out
    }
}

fn page_coverage_percent(chars: usize, baseline_chars: usize) -> f64 {
    if baseline_chars == 0 {
        100.0
    } else {
        (chars as f64 / baseline_chars as f64 * 100.0).min(100.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_does_not_describe_over_baseline_reconstruction_as_of_total() {
        let report = ConversionReport::build(
            "in.pdf",
            "out.epub",
            Vec::new(),
            ReportMetrics {
                blocks: 1,
                reconstructed_chars: 101,
                media_preserved_chars: 0,
                baseline_chars: 100,
                images: 0,
                figures: 0,
                tables: 0,
                equations: 0,
                layout_warnings: Vec::new(),
            },
        );

        assert_eq!(report.coverage_percent, 100.0);
        assert!(report.summary().contains(
            "101 reconstructed + 0 preserved as images + 0 removed as running heads and page numbers / 100 baseline characters"
        ));
        assert!(!report.summary().contains("101 of 100"));
    }

    fn page(number: u32, chars: usize, baseline: usize, running: usize) -> PageStats {
        PageStats {
            page: number,
            lines: usize::from(chars > 0),
            chars,
            baseline_chars: baseline,
            running_header_chars: running,
            two_column: false,
            rtl_dominant: false,
            image_only_candidate: false,
            low_confidence: false,
            low_confidence_action: None,
        }
    }

    fn metrics(reconstructed: usize, baseline: usize) -> ReportMetrics {
        ReportMetrics {
            blocks: 1,
            reconstructed_chars: reconstructed,
            media_preserved_chars: 0,
            baseline_chars: baseline,
            images: 0,
            figures: 0,
            tables: 0,
            equations: 0,
            layout_warnings: Vec::new(),
        }
    }

    #[test]
    fn removed_running_heads_are_credited_to_document_coverage() {
        // Short pages, each with a 20-character running head and folio:
        // the raw ratio is 80%, all of the deficit removed by design.
        let pages = (1..=3).map(|number| page(number, 80, 100, 20)).collect();
        let report = ConversionReport::build("in.pdf", "out.epub", pages, metrics(240, 300));

        assert_eq!(report.running_header_chars, 60);
        assert_eq!(report.coverage_percent, 100.0);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert!(report.summary().contains(
            "240 reconstructed + 0 preserved as images + 60 removed as running heads and page numbers / 300 baseline characters"
        ));
    }

    #[test]
    fn low_confidence_page_warnings_report_the_judged_coverage() {
        let mut flagged = page(2, 50, 100, 20);
        flagged.low_confidence = true;
        flagged.low_confidence_action = Some("linearize".to_string());
        let report = ConversionReport::build("in.pdf", "out.epub", vec![flagged], metrics(50, 100));

        assert!(
            report.warnings.iter().any(|warning| warning.starts_with(
                "page 2: low-confidence reconstruction (70.0% of pdftotext baseline, 50 reconstructed + 20 removed as running heads and page numbers / 100 baseline characters)"
            )),
            "{:?}",
            report.warnings
        );
    }

    #[test]
    fn coverage_is_unavailable_without_a_selectable_text_baseline() {
        let mut scan = page(1, 0, 0, 0);
        scan.image_only_candidate = true;
        scan.low_confidence = true;
        scan.low_confidence_action = Some("linearize".to_string());
        let report = ConversionReport::build("scan.pdf", "scan.epub", vec![scan], metrics(0, 0));

        assert!(!report.text_coverage_available);
        assert!(
            report
                .summary()
                .contains("Text coverage vs pdftotext: unavailable (no selectable-text baseline)")
        );
        assert!(!report.summary().contains("100.0%"));

        let mixed = vec![page(1, 0, 0, 0), page(2, 100, 100, 0)];
        let report = ConversionReport::build("mixed.pdf", "mixed.epub", mixed, metrics(100, 100));
        assert!(report.text_coverage_available);
        assert!(
            report
                .summary()
                .contains("Text coverage vs pdftotext: 100.0%")
        );
    }

    #[test]
    fn blank_pages_without_baseline_text_do_not_warn() {
        let report = ConversionReport::build(
            "in.pdf",
            "out.epub",
            vec![PageStats {
                page: 2,
                lines: 0,
                chars: 0,
                baseline_chars: 0,
                running_header_chars: 0,
                two_column: false,
                rtl_dominant: false,
                image_only_candidate: false,
                low_confidence: false,
                low_confidence_action: None,
            }],
            ReportMetrics {
                blocks: 0,
                reconstructed_chars: 0,
                media_preserved_chars: 0,
                baseline_chars: 0,
                images: 0,
                figures: 0,
                tables: 0,
                equations: 0,
                layout_warnings: Vec::new(),
            },
        );

        assert!(report.warnings.is_empty());
        assert!(report.summary().contains("Warnings: none"));
    }

    #[test]
    fn pages_with_baseline_text_but_no_reconstruction_warn() {
        let report = ConversionReport::build(
            "in.pdf",
            "out.epub",
            vec![PageStats {
                page: 7,
                lines: 0,
                chars: 0,
                baseline_chars: 42,
                running_header_chars: 0,
                two_column: false,
                rtl_dominant: false,
                image_only_candidate: false,
                low_confidence: false,
                low_confidence_action: None,
            }],
            ReportMetrics {
                blocks: 0,
                reconstructed_chars: 0,
                media_preserved_chars: 0,
                baseline_chars: 42,
                images: 0,
                figures: 0,
                tables: 0,
                equations: 0,
                layout_warnings: Vec::new(),
            },
        );

        assert_eq!(report.warnings.len(), 2);
        assert!(
            report.warnings[1].contains("page 7: no text reconstructed, but pdftotext found 42")
        );
    }

    #[test]
    fn pages_holding_only_removed_margins_do_not_warn() {
        let report = ConversionReport::build(
            "in.pdf",
            "out.epub",
            vec![PageStats {
                page: 36,
                lines: 0,
                chars: 0,
                baseline_chars: 13,
                running_header_chars: 13,
                two_column: false,
                rtl_dominant: false,
                image_only_candidate: false,
                low_confidence: false,
                low_confidence_action: None,
            }],
            ReportMetrics {
                blocks: 0,
                reconstructed_chars: 0,
                media_preserved_chars: 0,
                baseline_chars: 0,
                images: 0,
                figures: 0,
                tables: 0,
                equations: 0,
                layout_warnings: Vec::new(),
            },
        );

        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    }

    #[test]
    fn low_confidence_pages_warn_with_action() {
        let report = ConversionReport::build(
            "in.pdf",
            "out.epub",
            vec![PageStats {
                page: 4,
                lines: 1,
                chars: 4,
                baseline_chars: 100,
                running_header_chars: 0,
                two_column: false,
                rtl_dominant: false,
                image_only_candidate: false,
                low_confidence: true,
                low_confidence_action: Some("preserve".to_string()),
            }],
            ReportMetrics {
                blocks: 0,
                reconstructed_chars: 4,
                media_preserved_chars: 0,
                baseline_chars: 100,
                images: 0,
                figures: 1,
                tables: 0,
                equations: 0,
                layout_warnings: Vec::new(),
            },
        );

        assert_eq!(report.low_confidence_pages, 1);
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("page 4: low-confidence")
                    && warning.contains("action=preserve"))
        );
        assert!(report.summary().contains("Low-confidence pages: 1"));
    }

    #[test]
    fn ocr_recovered_pages_are_counted_and_summarized() {
        let report = ConversionReport::build(
            "in.pdf",
            "out.epub",
            vec![PageStats {
                page: 4,
                lines: 1,
                chars: 4,
                baseline_chars: 100,
                running_header_chars: 0,
                two_column: false,
                rtl_dominant: false,
                image_only_candidate: false,
                low_confidence: true,
                low_confidence_action: Some("ocr".to_string()),
            }],
            ReportMetrics {
                blocks: 1,
                reconstructed_chars: 4,
                media_preserved_chars: 0,
                baseline_chars: 100,
                images: 0,
                figures: 0,
                tables: 0,
                equations: 0,
                layout_warnings: Vec::new(),
            },
        );

        assert_eq!(report.ocr_pages, 1);
        assert!(report.summary().contains("OCR-recovered pages: 1"));
    }

    #[test]
    fn media_preserved_chars_credit_coverage() {
        let report = ConversionReport::build(
            "in.pdf",
            "out.epub",
            Vec::new(),
            ReportMetrics {
                blocks: 1,
                reconstructed_chars: 80,
                media_preserved_chars: 20,
                baseline_chars: 100,
                images: 0,
                figures: 1,
                tables: 1,
                equations: 0,
                layout_warnings: Vec::new(),
            },
        );

        assert_eq!(report.coverage_percent, 100.0);
        assert!(report.warnings.is_empty());
        assert!(report.summary().contains("20 preserved as images"));
    }

    #[test]
    fn summary_includes_layout_warnings() {
        let report = ConversionReport::build(
            "in.pdf",
            "out.epub",
            Vec::new(),
            ReportMetrics {
                blocks: 0,
                reconstructed_chars: 0,
                media_preserved_chars: 0,
                baseline_chars: 0,
                images: 0,
                figures: 1,
                tables: 0,
                equations: 0,
                layout_warnings: vec![
                    "page 1: lowercase paragraph continuation follows media block near y=120"
                        .to_string(),
                ],
            },
        );

        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("lowercase paragraph continuation"))
        );
        assert!(
            report
                .summary()
                .contains("lowercase paragraph continuation follows media block")
        );
    }
}
