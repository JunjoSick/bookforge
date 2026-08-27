//! In-process [`PopplerBackend`] fixture (TEST-2/PDF-2).
//!
//! Integration tests drive conversion end-to-end without spawning a
//! single process: each poppler surface (`pdftohtml`, `pdftotext`,
//! `pdfimages`, `pdftoppm`) is replaced by configurable in-memory
//! outcomes — success values, injected failures carrying the same exit
//! codes / deadline payloads real binaries produce, missing optional
//! tools — while any artifact that downstream code must read back from
//! disk (extracted images, rendered page PNGs) is materialized with the
//! same fs calls production uses. The scrubbed-environment allowlist,
//! pipe capture limits and wait/deadline arithmetic stay covered by the
//! current-executable stand-in tests in [`crate::tools`]; everything
//! above the backend seam runs here identically on Unix and Windows.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

use super::*;
use crate::tools::{PDF_RENDER_DPI, ToolError, write_solid_rgb_png};

/// One embedded image the fake `pdfimages` produces. When `contents` is
/// `None` the file is *advertised but never written*, reproducing an
/// inconsistent extraction so error paths read a dangling path.
pub(super) struct FixtureImage {
    pub(super) page: u32,
    pub(super) width: Option<u32>,
    pub(super) height: Option<u32>,
    pub(super) contents: Option<Vec<u8>>,
}

impl FixtureImage {
    pub(super) fn with_bytes(page: u32, width: u32, height: u32, contents: &[u8]) -> Self {
        Self {
            page,
            width: Some(width),
            height: Some(height),
            contents: Some(contents.to_vec()),
        }
    }

    /// Listed but never materialized on disk.
    pub(super) fn dangling(page: u32, width: u32, height: u32) -> Self {
        Self {
            page,
            width: Some(width),
            height: Some(height),
            contents: None,
        }
    }
}

enum ImagesOutcome {
    /// Optional tool absent from `PATH` entirely.
    Absent,
    Extraction(Vec<FixtureImage>),
}

enum XmlOutcome {
    Ok(String),
    /// `pdftohtml` overran its deadline.
    TimedOut(Duration),
}

enum BaselineOutcome {
    Ok(String),
    /// `pdftotext` exited non-zero with diagnostics.
    Failed {
        code: i32,
        stderr: String,
    },
}

/// Configurable fake covering the whole [`PopplerBackend`] surface.
pub(super) struct FakePoppler {
    xml: XmlOutcome,
    text: BaselineOutcome,
    images: ImagesOutcome,
    render_stderr: Option<String>,
    calls: Mutex<HashMap<&'static str, usize>>,
}

impl FakePoppler {
    /// Successful `pdftohtml` XML plus successful `pdftotext` baseline.
    pub(super) fn new(xml: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            xml: XmlOutcome::Ok(xml.into()),
            text: BaselineOutcome::Ok(text.into()),
            images: ImagesOutcome::Extraction(Vec::new()),
            render_stderr: None,
            calls: Mutex::new(HashMap::new()),
        }
    }

    /// Make `pdftotext` fail with the given exit status, as a crashed
    /// binary would.
    pub(super) fn failing_baseline(mut self, code: i32, stderr: &str) -> Self {
        self.text = BaselineOutcome::Failed {
            code,
            stderr: stderr.to_string(),
        };
        self
    }

    /// Make `pdftohtml` overrun its deadline.
    pub(super) fn timing_out_xml(mut self, timeout: Duration) -> Self {
        self.xml = XmlOutcome::TimedOut(timeout);
        self
    }

    /// No `pdfimages` binary installed (optional-tool degradation).
    pub(super) fn without_image_tool(mut self) -> Self {
        self.images = ImagesOutcome::Absent;
        self
    }

    pub(super) fn with_extracted_image(mut self, image: FixtureImage) -> Self {
        let ImagesOutcome::Extraction(images) = &mut self.images else {
            return self;
        };
        images.push(image);
        self
    }

    /// Raster rendering fails, e.g. a broken `pdftoppm`.
    pub(super) fn with_render_failure(mut self, stderr: &str) -> Self {
        self.render_stderr = Some(stderr.to_string());
        self
    }

    pub(super) fn call_count(&self, surface: &str) -> usize {
        self.calls
            .lock()
            .expect("fake call map")
            .get(surface)
            .copied()
            .unwrap_or(0)
    }

    fn count(&self, surface: &'static str) {
        *self
            .calls
            .lock()
            .expect("fake call map")
            .entry(surface)
            .or_default() += 1;
    }
}

impl PopplerBackend for FakePoppler {
    fn pdf_to_xml(&self, _pdf: &Path) -> std::result::Result<String, ToolError> {
        self.count("pdf_to_xml");
        match &self.xml {
            XmlOutcome::Ok(xml) => Ok(xml.clone()),
            XmlOutcome::TimedOut(timeout) => Err(ToolError::TimedOut {
                tool: "pdftohtml",
                timeout: *timeout,
            }),
        }
    }

    fn pdf_to_text(&self, _pdf: &Path) -> std::result::Result<String, ToolError> {
        self.count("pdf_to_text");
        match &self.text {
            BaselineOutcome::Ok(text) => Ok(text.clone()),
            BaselineOutcome::Failed { code, stderr } => Err(ToolError::Failed {
                tool: "pdftotext",
                code: Some(*code),
                stderr: stderr.clone(),
            }),
        }
    }

    fn extract_images(
        &self,
        _pdf: &Path,
        output_dir: &Path,
    ) -> std::result::Result<Vec<ExtractedImage>, ToolError> {
        self.count("extract_images");
        let ImagesOutcome::Extraction(images) = &self.images else {
            return Err(ToolError::NotFound("pdfimages"));
        };
        fs::create_dir_all(output_dir)?;
        let mut extracted = Vec::with_capacity(images.len());
        for (index, image) in images.iter().enumerate() {
            let path = output_dir.join(format!("fixture-{:04}-{index}.png", image.page));
            if let Some(contents) = &image.contents {
                fs::write(&path, contents)?;
            }
            extracted.push(ExtractedImage {
                page: image.page,
                index,
                width: image.width,
                height: image.height,
                path,
                extension: "png".to_string(),
            });
        }
        Ok(extracted)
    }

    fn render_page_png(
        &self,
        pdf: &Path,
        page: u32,
        output_dir: &Path,
    ) -> std::result::Result<PathBuf, ToolError> {
        self.render_page_png_scaled(pdf, page, output_dir, PDF_RENDER_DPI)
    }

    fn render_page_png_scaled(
        &self,
        _pdf: &Path,
        page: u32,
        output_dir: &Path,
        _dpi: u32,
    ) -> std::result::Result<PathBuf, ToolError> {
        self.count("render_page");
        if let Some(stderr) = &self.render_stderr {
            return Err(ToolError::Failed {
                tool: "pdftoppm",
                code: Some(1),
                stderr: stderr.clone(),
            });
        }
        fs::create_dir_all(output_dir)?;
        let path = output_dir.join(format!("page-{page:04}.png"));
        // A solid raster real enough for crop_png_to_file arithmetic;
        // downstream crops work off returned paths, not DPI bookkeeping.
        write_solid_rgb_png(&path, 240, 320, [240, 240, 240])?;
        Ok(path)
    }
}
