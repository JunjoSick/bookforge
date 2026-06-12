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
    pub page_stats: Vec<PageStats>,
    pub warnings: Vec<String>,
}

impl ConversionReport {
    pub fn build(
        input: &str,
        output: &str,
        page_stats: Vec<PageStats>,
        blocks: usize,
        reconstructed_chars: usize,
        baseline_chars: usize,
    ) -> Self {
        let coverage_percent = if baseline_chars == 0 {
            100.0
        } else {
            (reconstructed_chars as f64 / baseline_chars as f64 * 100.0).min(100.0)
        };

        let mut warnings = Vec::new();
        if coverage_percent < 95.0 {
            warnings.push(format!(
                "reconstructed text covers only {coverage_percent:.1}% of the pdftotext baseline; some content was not captured"
            ));
        }
        for page in &page_stats {
            if page.chars == 0 {
                warnings.push(format!(
                    "page {}: no text reconstructed (image-only page, or extraction failure)",
                    page.page
                ));
            }
        }

        Self {
            input: input.to_string(),
            output: output.to_string(),
            pages: page_stats.len(),
            blocks,
            reconstructed_chars,
            baseline_chars,
            coverage_percent,
            two_column_pages: page_stats.iter().filter(|page| page.two_column).count(),
            page_stats,
            warnings,
        }
    }

    pub fn summary(&self) -> String {
        let mut out = format!(
            "Pages: {}\nBlocks: {}\nTwo-column pages: {}\nText coverage vs pdftotext: {:.1}% ({} of {} characters)\n",
            self.pages,
            self.blocks,
            self.two_column_pages,
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
