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
    pub page_stats: Vec<PageStats>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct ReportMetrics {
    pub blocks: usize,
    pub reconstructed_chars: usize,
    pub baseline_chars: usize,
    pub images: usize,
    pub figures: usize,
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
            if page.chars == 0 && page.baseline_chars > 0 {
                warnings.push(format!(
                    "page {}: no text reconstructed, but pdftotext found {} baseline characters",
                    page.page, page.baseline_chars
                ));
            }
        }

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
            page_stats,
            warnings,
        }
    }

    pub fn summary(&self) -> String {
        let mut out = format!(
            "Pages: {}\nBlocks: {}\nTwo-column pages: {}\nImages: {} extracted, {} figure block(s)\nText coverage vs pdftotext: {:.1}% ({} reconstructed / {} baseline characters)\n",
            self.pages,
            self.blocks,
            self.two_column_pages,
            self.images,
            self.figures,
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
            }],
            ReportMetrics {
                blocks: 0,
                reconstructed_chars: 0,
                baseline_chars: 0,
                images: 0,
                figures: 0,
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
            }],
            ReportMetrics {
                blocks: 0,
                reconstructed_chars: 0,
                baseline_chars: 42,
                images: 0,
                figures: 0,
            },
        );

        assert_eq!(report.warnings.len(), 2);
        assert!(
            report.warnings[1].contains("page 7: no text reconstructed, but pdftotext found 42")
        );
    }
}
