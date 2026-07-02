//! Discovery and invocation of poppler command-line tools.
//!
//! Resolution order: `POPPLER_PATH` environment variable (a directory
//! containing the binaries), then the system `PATH`. External binaries
//! follow the EPUBCheck precedent (ROADMAP §1.6, §8.4): subprocesses
//! are acceptable, embedded runtimes are not.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
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
    pub pdfimages: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ExtractedImage {
    pub page: u32,
    pub index: usize,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub path: PathBuf,
    pub extension: String,
}

impl PopplerTools {
    /// Locate the required poppler binaries or explain what is missing.
    pub fn discover() -> Result<Self, ToolError> {
        Ok(Self {
            pdftohtml: find_tool("pdftohtml")?,
            pdftotext: find_tool("pdftotext")?,
            pdfimages: find_tool("pdfimages")?,
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
        let work_dir = scoped_temp_dir("bookforge-pdftohtml")?;
        let pdf = absolute_path(pdf)?;
        let output = Command::new(&self.pdftohtml)
            .current_dir(&work_dir)
            .args(["-xml", "-stdout", "-q", "-enc", "UTF-8", "-fmt", "png"])
            .arg(pdf)
            .output()?;
        let _ = fs::remove_dir_all(&work_dir);
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

    pub fn extract_images(
        &self,
        pdf: &Path,
        output_dir: &Path,
    ) -> Result<Vec<ExtractedImage>, ToolError> {
        fs::create_dir_all(output_dir)?;
        let listed = self.list_images(pdf)?;
        if listed.is_empty() {
            return Ok(Vec::new());
        }

        let root = output_dir.join("image");
        let output = Command::new(&self.pdfimages)
            .args(["-png", "-p", "-print-filenames", "-q"])
            .arg(pdf)
            .arg(&root)
            .output()?;
        if !output.status.success() {
            return Err(ToolError::Failed {
                tool: "pdfimages",
                code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        let mut paths = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| PathBuf::from(line.trim()))
            .filter(|path| !path.as_os_str().is_empty())
            .collect::<Vec<_>>();
        if paths.is_empty() {
            paths = extracted_image_paths(output_dir)?;
        }
        paths.sort();

        Ok(paths
            .into_iter()
            .enumerate()
            .map(|(index, path)| {
                let listed = listed.get(index);
                ExtractedImage {
                    page: listed.map(|image| image.page).unwrap_or(0),
                    index,
                    width: listed.and_then(|image| image.width),
                    height: listed.and_then(|image| image.height),
                    extension: path
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .unwrap_or("png")
                        .to_ascii_lowercase(),
                    path,
                }
            })
            .collect())
    }

    fn list_images(&self, pdf: &Path) -> Result<Vec<ListedImage>, ToolError> {
        let output = Command::new(&self.pdfimages)
            .args(["-list"])
            .arg(pdf)
            .output()?;
        if !output.status.success() {
            return Err(ToolError::Failed {
                tool: "pdfimages",
                code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        Ok(parse_pdfimages_list(&String::from_utf8_lossy(
            &output.stdout,
        )))
    }
}

#[derive(Debug, Clone, Copy)]
struct ListedImage {
    page: u32,
    width: Option<u32>,
    height: Option<u32>,
}

fn parse_pdfimages_list(output: &str) -> Vec<ListedImage> {
    output
        .lines()
        .filter_map(|line| {
            let columns = line.split_whitespace().collect::<Vec<_>>();
            let page = columns.first()?.parse::<u32>().ok()?;
            if columns.get(2) != Some(&"image") {
                return None;
            }
            let width = columns.get(3).and_then(|value| value.parse::<u32>().ok());
            let height = columns.get(4).and_then(|value| value.parse::<u32>().ok());
            Some(ListedImage {
                page,
                width,
                height,
            })
        })
        .collect()
}

fn extracted_image_paths(output_dir: &Path) -> Result<Vec<PathBuf>, ToolError> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(output_dir)? {
        let path = entry?.path();
        let is_image = path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| matches!(ext.to_ascii_lowercase().as_str(), "png" | "jpg" | "jpeg"));
        if is_image {
            paths.push(path);
        }
    }
    Ok(paths)
}

fn absolute_path(path: &Path) -> Result<PathBuf, ToolError> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(std::env::current_dir()?.join(path))
}

fn scoped_temp_dir(prefix: &str) -> Result<PathBuf, ToolError> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let path = std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&path)?;
    Ok(path)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pdfimages_list_rows() {
        let rows = parse_pdfimages_list(
            r#"page   num  type   width height color comp bpc  enc interp  object ID x-ppi y-ppi size ratio
--------------------------------------------------------------------------------------------
   1     0 image     640   480  rgb     3   8  image  no        12  0    72    72  12K 2.0%
   3     1 smask     120    80  gray    1   8  image  no        13  0    72    72  1K  1.0%
"#,
        );

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].page, 1);
        assert_eq!(rows[0].width, Some(640));
    }
}
