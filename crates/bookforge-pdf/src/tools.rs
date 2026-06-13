//! Discovery and invocation of poppler command-line tools.
//!
//! Resolution order: `POPPLER_PATH` environment variable (a directory
//! containing the binaries), then the system `PATH`. External binaries
//! follow the EPUBCheck precedent (ROADMAP §1.6, §8.4): subprocesses
//! are acceptable, embedded runtimes are not.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error(
        "poppler tool '{0}' not found. Install poppler and either add it to PATH or set POPPLER_PATH to its bin directory."
    )]
    NotFound(&'static str),

    #[error("'{tool}' failed (exit {code:?}): {stderr}")]
    Failed {
        tool: &'static str,
        code: Option<i32>,
        stderr: String,
    },

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct PopplerTools {
    pub pdftohtml: PathBuf,
    pub pdftotext: PathBuf,
}

impl PopplerTools {
    /// Locate the required poppler binaries or explain what is missing.
    pub fn discover() -> Result<Self, ToolError> {
        Ok(Self {
            pdftohtml: find_tool("pdftohtml")?,
            pdftotext: find_tool("pdftotext")?,
        })
    }

    /// `pdftohtml -v` prints its version banner on stderr.
    pub fn version(&self) -> Option<String> {
        let output = Command::new(&self.pdftohtml).arg("-v").output().ok()?;
        let banner = String::from_utf8_lossy(&output.stderr);
        banner.lines().next().map(|line| line.trim().to_string())
    }

    /// Run `pdftohtml -xml` and return the XML document.
    pub fn pdf_to_xml(&self, pdf: &Path) -> Result<String, ToolError> {
        let output = Command::new(&self.pdftohtml)
            .args(["-xml", "-i", "-stdout", "-q", "-enc", "UTF-8"])
            .arg(pdf)
            .output()?;
        if !output.status.success() {
            return Err(ToolError::Failed {
                tool: "pdftohtml",
                code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Raw text via `pdftotext`, used as the coverage baseline: it makes
    /// no layout decisions, so its character count approximates "all the
    /// text poppler can see".
    pub fn pdf_to_text(&self, pdf: &Path) -> Result<String, ToolError> {
        let output = Command::new(&self.pdftotext)
            .args(["-enc", "UTF-8", "-q"])
            .arg(pdf)
            .arg("-")
            .output()?;
        if !output.status.success() {
            return Err(ToolError::Failed {
                tool: "pdftotext",
                code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

fn find_tool(name: &'static str) -> Result<PathBuf, ToolError> {
    let exe = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };

    if let Ok(dir) = std::env::var("POPPLER_PATH") {
        let candidate = Path::new(&dir).join(&exe);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(&exe);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    Err(ToolError::NotFound(name))
}
