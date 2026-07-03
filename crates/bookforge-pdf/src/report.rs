//! Conversion fidelity report. The contract from ROADMAP §9b: pages
//! that reconstruct badly are flagged, never hidden.

use serde::Serialize;

use crate::reconstruct::PageStats;

#[derive(Debug, Serialize)]
pub struct ConversionReport {
    pub input: String,
    pub output: String,
    pub pages: usize,
    pub blocks: usize,
    /// Non-whitespace characters in reconstructed blocks.
    pub reconstructed_chars: usize,
    /// Non-whitespace characters in the raw `pdftotext` baseline.
    pub baseline_chars: usize,
    /// reconstructed/baseline, capped at 100. Above ~100 means the
    /// baseline missed text (rare); far below means reconstruction
    /// dropped content and the page list below says where.
    pub coverage_percent: f64,
    pub two_column_pages: usize,
    pub images: usize,
    pub figures: usize,
    pub tables: usize,
    pub equations: usize,
    pub low_confidence_pages: usize,
    pub page_stats: Vec<PageStats>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ReportMetrics {
    pub blocks: usize,
    pub reconstructed_chars: usize,
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
        let coverage_percent = if metrics.baseline_chars == 0 {
            100.0
        } else {
            (metrics.reconstructed_chars as f64 / metrics.baseline_chars as f64 * 100.0).min(100.0)
        };

        let mut warnings = Vec::new();
        if coverage_percent < 95.0 {
            warnings.push(format!(
                "reconstructed text covers only {coverage_percent:.1}% of the pdftotext baseline; some content was not captured"
            ));
        }
        for page in &page_stats {
            if page.low_confidence {
                let action = page.low_confidence_action.as_deref().unwrap_or("linearize");
                let page_coverage = page_coverage_percent(page.chars, page.baseline_chars);
                warnings.push(format!(
                    "page {}: low-confidence reconstruction ({page_coverage:.1}% of pdftotext baseline, {} reconstructed / {} baseline characters); action={action}",
                    page.page, page.chars, page.baseline_chars
                ));
            } else if page.chars == 0 && page.baseline_chars > 0 {
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
            baseline_chars: metrics.baseline_chars,
            coverage_percent,
            two_column_pages: page_stats.iter().filter(|page| page.two_column).count(),
            images: metrics.images,
            figures: metrics.figures,
            tables: metrics.tables,
            equations: metrics.equations,
            low_confidence_pages: page_stats.iter().filter(|page| page.low_confidence).count(),
            page_stats,
            warnings,
        }
    }

    pub fn summary(&self) -> String {
        let mut out = format!(
            "Pages: {}\nBlocks: {}\nTwo-column pages: {}\nImages: {} extracted, {} figure block(s)\nTables: {} crop(s)\nEquations: {} crop(s)\nLow-confidence pages: {}\nText coverage vs pdftotext: {:.1}% ({} reconstructed / {} baseline characters)\n",
            self.pages,
            self.blocks,
            self.two_column_pages,
            self.images,
            self.figures,
            self.tables,
            self.equations,
            self.low_confidence_pages,
            self.coverage_percent,
            self.reconstructed_chars,
            self.baseline_chars,
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
                baseline_chars: 100,
                images: 0,
                figures: 0,
                tables: 0,
                equations: 0,
                layout_warnings: Vec::new(),
            },
        );

        assert_eq!(report.coverage_percent, 100.0);
        assert!(
            report
                .summary()
                .contains("101 reconstructed / 100 baseline characters")
        );
        assert!(!report.summary().contains("101 of 100"));
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
                two_column: false,
                low_confidence: false,
                low_confidence_action: None,
            }],
            ReportMetrics {
                blocks: 0,
                reconstructed_chars: 0,
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
                two_column: false,
                low_confidence: false,
                low_confidence_action: None,
            }],
            ReportMetrics {
                blocks: 0,
                reconstructed_chars: 0,
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
    fn low_confidence_pages_warn_with_action() {
        let report = ConversionReport::build(
            "in.pdf",
            "out.epub",
            vec![PageStats {
                page: 4,
                lines: 1,
                chars: 4,
                baseline_chars: 100,
                two_column: false,
                low_confidence: true,
                low_confidence_action: Some("preserve".to_string()),
            }],
            ReportMetrics {
                blocks: 0,
                reconstructed_chars: 4,
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
    fn summary_includes_layout_warnings() {
        let report = ConversionReport::build(
            "in.pdf",
            "out.epub",
            Vec::new(),
            ReportMetrics {
                blocks: 0,
                reconstructed_chars: 0,
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
