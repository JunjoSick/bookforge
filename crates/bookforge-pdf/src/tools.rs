//! Discovery and invocation of poppler command-line tools.
//!
//! Resolution order: `POPPLER_PATH` environment variable (a directory
//! containing the binaries), then the system `PATH`. External binaries
//! follow the EPUBCheck precedent (ROADMAP §1.6, §8.4): subprocesses
//! are acceptable, embedded runtimes are not.

use std::{
    collections::HashMap,
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use flate2::{Compression, read::ZlibDecoder, write::ZlibEncoder};

pub const PDF_RENDER_DPI: u32 = 150;
const PDF_XML_ZOOM: &str = "1.5";
const PDF_XML_ZOOM_NUM: i64 = 3;
const PDF_XML_ZOOM_DEN: i64 = 2;
const PDF_POINTS_PER_INCH: i64 = 72;
pub const DEFAULT_POPPLER_TIMEOUT: Duration = Duration::from_secs(120);
/// XML from large, layout-heavy PDFs can be substantial, so stdout gets a
/// generous ceiling while still preventing an external tool from exhausting RAM.
const MAX_POPPLER_STDOUT_BYTES: usize = 256 * 1024 * 1024;
/// Poppler stderr is only included in diagnostics and should remain compact.
const MAX_POPPLER_STDERR_BYTES: usize = 64 * 1024;
const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(10);
const TEMP_DIR_RANDOM_BYTES: usize = 16;
const TEMP_DIR_CREATE_ATTEMPTS: usize = 128;

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

    #[error("'{tool}' timed out after {timeout:?}")]
    TimedOut {
        tool: &'static str,
        timeout: Duration,
    },

    #[error("'{tool}' {stream} exceeded the {limit}-byte capture limit")]
    OutputTooLarge {
        tool: &'static str,
        stream: &'static str,
        limit: usize,
    },

    #[error("pdfimages output did not match its -list rows: {0}")]
    ImageListMismatch(String),

    #[error("unsupported PNG produced by pdftoppm: {0}")]
    UnsupportedPng(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct PopplerTools {
    pub pdftohtml: PathBuf,
    pub pdftotext: PathBuf,
    pub pdfimages: Option<PathBuf>,
    pub pdftoppm: Option<PathBuf>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageCrop {
    pub page: u32,
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
}

impl PageCrop {
    pub(crate) fn to_render_pixels(self) -> Self {
        Self {
            page: self.page,
            left: scale_xml_to_render_px(self.left).max(0),
            top: scale_xml_to_render_px(self.top).max(0),
            width: scale_xml_to_render_px(self.width).max(1),
            height: scale_xml_to_render_px(self.height).max(1),
        }
    }
}

impl PopplerTools {
    /// Locate the required poppler binaries or explain what is missing.
    pub fn discover() -> Result<Self, ToolError> {
        Ok(Self {
            pdftohtml: find_tool("pdftohtml")?,
            pdftotext: find_tool("pdftotext")?,
            pdfimages: find_tool("pdfimages").ok(),
            pdftoppm: find_tool("pdftoppm").ok(),
        })
    }

    fn pdfimages_path(&self) -> Result<&Path, ToolError> {
        self.pdfimages
            .as_deref()
            .ok_or(ToolError::NotFound("pdfimages"))
    }

    fn pdftoppm_path(&self) -> Result<&Path, ToolError> {
        self.pdftoppm
            .as_deref()
            .ok_or(ToolError::NotFound("pdftoppm"))
    }

    /// `pdftohtml -v` prints its version banner on stderr.
    pub fn version(&self) -> Option<String> {
        self.version_with_timeout(DEFAULT_POPPLER_TIMEOUT)
    }

    /// Return the Poppler version banner, bounding this invocation by `timeout`.
    pub fn version_with_timeout(&self, timeout: Duration) -> Option<String> {
        let mut command = poppler_command(&self.pdftohtml);
        command.arg("-v");
        let output = command_output(&mut command, "pdftohtml", timeout).ok()?;
        let banner = String::from_utf8_lossy(&output.stderr);
        banner.lines().next().map(|line| line.trim().to_string())
    }

    /// Run `pdftohtml -xml` and return the XML document.
    pub fn pdf_to_xml(&self, pdf: &Path) -> Result<String, ToolError> {
        self.pdf_to_xml_with_timeout(pdf, DEFAULT_POPPLER_TIMEOUT)
    }

    /// Run `pdftohtml -xml` with a caller-selected deadline.
    pub fn pdf_to_xml_with_timeout(
        &self,
        pdf: &Path,
        timeout: Duration,
    ) -> Result<String, ToolError> {
        let work_dir = scoped_temp_dir("bookforge-pdftohtml")?;
        let pdf = absolute_path(pdf)?;
        let result = (|| {
            let mut command = poppler_command(&self.pdftohtml);
            command.current_dir(&work_dir).args([
                "-xml",
                "-stdout",
                "-q",
                "-enc",
                "UTF-8",
                "-fmt",
                "png",
                "-zoom",
                PDF_XML_ZOOM,
            ]);
            command.arg(pdf);
            let output = command_output(&mut command, "pdftohtml", timeout)?;
            if !output.status.success() {
                return Err(ToolError::Failed {
                    tool: "pdftohtml",
                    code: output.status.code(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                });
            }
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        })();
        let _ = fs::remove_dir_all(&work_dir);
        result
    }

    /// Raw text via `pdftotext`, used as the coverage baseline: it makes
    /// no layout decisions, so its character count approximates "all the
    /// text poppler can see".
    pub fn pdf_to_text(&self, pdf: &Path) -> Result<String, ToolError> {
        self.pdf_to_text_with_timeout(pdf, DEFAULT_POPPLER_TIMEOUT)
    }

    /// Run `pdftotext` with a caller-selected deadline.
    pub fn pdf_to_text_with_timeout(
        &self,
        pdf: &Path,
        timeout: Duration,
    ) -> Result<String, ToolError> {
        let mut command = poppler_command(&self.pdftotext);
        command.args(["-enc", "UTF-8", "-q"]).arg(pdf).arg("-");
        let output = command_output(&mut command, "pdftotext", timeout)?;
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
        self.extract_images_with_timeout(pdf, output_dir, DEFAULT_POPPLER_TIMEOUT)
    }

    /// Extract embedded images with a caller-selected deadline per Poppler call.
    pub fn extract_images_with_timeout(
        &self,
        pdf: &Path,
        output_dir: &Path,
        timeout: Duration,
    ) -> Result<Vec<ExtractedImage>, ToolError> {
        fs::create_dir_all(output_dir)?;
        let listed = self.list_images(pdf, timeout)?;
        if listed.is_empty() {
            return Ok(Vec::new());
        }

        let root = output_dir.join("image");
        let mut command = poppler_command(self.pdfimages_path()?);
        command
            .args(["-png", "-p", "-print-filenames", "-q"])
            .arg(pdf)
            .arg(&root);
        let output = command_output(&mut command, "pdfimages", timeout)?;
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
        pair_extracted_images_with_list_rows(paths, &listed)
    }

    pub fn render_page_png(
        &self,
        pdf: &Path,
        page: u32,
        output_dir: &Path,
    ) -> Result<PathBuf, ToolError> {
        self.render_page_png_with_timeout(pdf, page, output_dir, DEFAULT_POPPLER_TIMEOUT)
    }

    /// Render one page with a caller-selected deadline.
    pub fn render_page_png_with_timeout(
        &self,
        pdf: &Path,
        page: u32,
        output_dir: &Path,
        timeout: Duration,
    ) -> Result<PathBuf, ToolError> {
        fs::create_dir_all(output_dir)?;
        let root = output_dir.join(format!("page-{page:04}"));
        let page_arg = page.to_string();
        let dpi_arg = PDF_RENDER_DPI.to_string();
        let mut command = poppler_command(self.pdftoppm_path()?);
        command
            .args([
                "-f",
                &page_arg,
                "-l",
                &page_arg,
                "-singlefile",
                "-png",
                "-r",
                &dpi_arg,
            ])
            .arg(pdf)
            .arg(&root);
        let output = command_output(&mut command, "pdftoppm", timeout)?;
        if !output.status.success() {
            return Err(ToolError::Failed {
                tool: "pdftoppm",
                code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        Ok(root.with_extension("png"))
    }

    pub fn render_page_crop_png(
        &self,
        pdf: &Path,
        crop: PageCrop,
        output_dir: &Path,
        name: &str,
    ) -> Result<PathBuf, ToolError> {
        self.render_page_crop_png_with_timeout(pdf, crop, output_dir, name, DEFAULT_POPPLER_TIMEOUT)
    }

    /// Render one page crop with a caller-selected deadline.
    pub fn render_page_crop_png_with_timeout(
        &self,
        pdf: &Path,
        crop: PageCrop,
        output_dir: &Path,
        name: &str,
        timeout: Duration,
    ) -> Result<PathBuf, ToolError> {
        fs::create_dir_all(output_dir)?;
        let root = output_dir.join(name);
        let crop = crop.to_render_pixels();
        let page_arg = crop.page.to_string();
        let left_arg = crop.left.to_string();
        let top_arg = crop.top.to_string();
        let width_arg = crop.width.to_string();
        let height_arg = crop.height.to_string();
        let dpi_arg = PDF_RENDER_DPI.to_string();
        let mut command = poppler_command(self.pdftoppm_path()?);
        command
            .args([
                "-f",
                &page_arg,
                "-l",
                &page_arg,
                "-singlefile",
                "-png",
                "-r",
                &dpi_arg,
                "-x",
                &left_arg,
                "-y",
                &top_arg,
                "-W",
                &width_arg,
                "-H",
                &height_arg,
            ])
            .arg(pdf)
            .arg(&root);
        let output = command_output(&mut command, "pdftoppm", timeout)?;
        if !output.status.success() {
            return Err(ToolError::Failed {
                tool: "pdftoppm",
                code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        Ok(root.with_extension("png"))
    }

    fn list_images(&self, pdf: &Path, timeout: Duration) -> Result<Vec<ListedImage>, ToolError> {
        let mut command = poppler_command(self.pdfimages_path()?);
        command.args(["-list"]).arg(pdf);
        let output = command_output(&mut command, "pdfimages", timeout)?;
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

fn poppler_command(program: &Path) -> Command {
    let mut command = Command::new(program);
    command.env_clear();
    copy_environment_variable(&mut command, "PATH");
    #[cfg(windows)]
    {
        // Windows needs these for system DLL discovery and temporary-file APIs.
        copy_environment_variable(&mut command, "SYSTEMROOT");
        copy_environment_variable(&mut command, "TEMP");
        copy_environment_variable(&mut command, "TMP");
    }
    #[cfg(unix)]
    {
        // None of these are secrets, and poppler misbehaves without them:
        // `LD_LIBRARY_PATH` is how a non-system build (Homebrew, nix, conda,
        // a local prefix) finds libpoppler; fontconfig — which the rendering
        // tools use for font matching — reads `HOME`, `XDG_CACHE_HOME` and
        // `FONTCONFIG_PATH`; and text extraction is locale-sensitive. Clearing
        // them would trade a key leak for broken conversions on exactly the
        // platforms this scrub is meant to protect. The point of the allowlist
        // is to withhold provider credentials, not to blank the environment.
        for name in [
            "LD_LIBRARY_PATH",
            "HOME",
            "LANG",
            "LC_ALL",
            "LC_CTYPE",
            "XDG_CACHE_HOME",
            "FONTCONFIG_PATH",
            "FONTCONFIG_FILE",
            "TMPDIR",
        ] {
            copy_environment_variable(&mut command, name);
        }
    }
    command
}

fn copy_environment_variable(command: &mut Command, name: &'static str) {
    if let Some(value) = std::env::var_os(name) {
        command.env(name, value);
    }
}

fn command_output(
    command: &mut Command,
    tool: &'static str,
    timeout: Duration,
) -> Result<Output, ToolError> {
    command_output_with_limits(
        command,
        tool,
        timeout,
        MAX_POPPLER_STDOUT_BYTES,
        MAX_POPPLER_STDERR_BYTES,
    )
}

fn command_output_with_limits(
    command: &mut Command,
    tool: &'static str,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
) -> Result<Output, ToolError> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = spawn_command(command)?;
    let mut child = ChildGuard::new(child);
    let stdout = child
        .stdout()
        .ok_or_else(|| io::Error::other("child stdout was not piped"))?;
    let stderr = child
        .stderr()
        .ok_or_else(|| io::Error::other("child stderr was not piped"))?;
    let stdout_reader = spawn_bounded_reader(stdout, stdout_limit, "stdout")?;
    let stderr_reader = spawn_bounded_reader(stderr, stderr_limit, "stderr")?;

    let wait_result = wait_with_timeout(&mut child, timeout);
    if wait_result.is_err() {
        child.kill_and_reap();
    }
    let stdout = join_reader(stdout_reader)?;
    let stderr = join_reader(stderr_reader)?;
    let status = match wait_result {
        Ok(status) => status,
        Err(WaitError::TimedOut) => {
            return Err(ToolError::TimedOut { tool, timeout });
        }
        Err(WaitError::Io(err)) => return Err(ToolError::Io(err)),
    };

    if stdout.exceeded {
        return Err(ToolError::OutputTooLarge {
            tool,
            stream: "stdout",
            limit: stdout_limit,
        });
    }

    Ok(Output {
        status,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
    })
}

fn spawn_command(command: &mut Command) -> io::Result<Child> {
    let mut delay = Duration::from_millis(5);
    for attempt in 0..4 {
        match command.spawn() {
            Ok(child) => return Ok(child),
            Err(err) if err.raw_os_error() == Some(26) && attempt < 3 => {
                thread::sleep(delay);
                delay *= 2;
            }
            Err(err) => return Err(err),
        }
    }
    command.spawn()
}

#[derive(Debug)]
struct CapturedOutput {
    bytes: Vec<u8>,
    exceeded: bool,
}

fn spawn_bounded_reader<R>(
    mut reader: R,
    limit: usize,
    stream: &'static str,
) -> io::Result<thread::JoinHandle<io::Result<CapturedOutput>>>
where
    R: Read + Send + 'static,
{
    thread::Builder::new()
        .name(format!("poppler-{stream}-reader"))
        .spawn(move || {
            let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
            let mut exceeded = false;
            let mut buffer = [0_u8; 16 * 1024];
            loop {
                let count = reader.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                let remaining = limit.saturating_sub(bytes.len());
                let retained = count.min(remaining);
                bytes.extend_from_slice(&buffer[..retained]);
                exceeded |= retained < count;
            }
            Ok(CapturedOutput { bytes, exceeded })
        })
}

fn join_reader(
    reader: thread::JoinHandle<io::Result<CapturedOutput>>,
) -> io::Result<CapturedOutput> {
    reader
        .join()
        .map_err(|_| io::Error::other("poppler output reader thread panicked"))?
}

enum WaitError {
    TimedOut,
    Io(io::Error),
}

fn wait_with_timeout(child: &mut ChildGuard, timeout: Duration) -> Result<ExitStatus, WaitError> {
    let started = Instant::now();
    loop {
        match child.try_wait().map_err(WaitError::Io)? {
            Some(status) => return Ok(status),
            None if started.elapsed() >= timeout => return Err(WaitError::TimedOut),
            None => {
                let delay = timeout
                    .saturating_sub(started.elapsed())
                    .min(CHILD_POLL_INTERVAL);
                if delay.is_zero() {
                    return Err(WaitError::TimedOut);
                }
                thread::sleep(delay);
            }
        }
    }
}

struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.child.as_mut()?.stdout.take()
    }

    fn stderr(&mut self) -> Option<std::process::ChildStderr> {
        self.child.as_mut()?.stderr.take()
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let status = self
            .child
            .as_mut()
            .expect("child guard must remain armed while waiting")
            .try_wait()?;
        if status.is_some() {
            self.child.take();
        }
        Ok(status)
    }

    fn kill_and_reap(&mut self) {
        let Some(child) = self.child.as_mut() else {
            return;
        };
        if !matches!(child.try_wait(), Ok(Some(_))) {
            let _ = child.kill();
            if child.wait().is_err() {
                return;
            }
        }
        self.child.take();
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.kill_and_reap();
    }
}

fn scale_xml_to_render_px(value: i32) -> i32 {
    let numerator = value as i64 * PDF_RENDER_DPI as i64 * PDF_XML_ZOOM_DEN;
    let denominator = PDF_POINTS_PER_INCH * PDF_XML_ZOOM_NUM;
    ((numerator + denominator / 2) / denominator) as i32
}

#[derive(Debug)]
struct PngImage {
    width: u32,
    height: u32,
    color_type: u8,
    bit_depth: u8,
    bytes_per_pixel: usize,
    pixels: Vec<u8>,
}

pub(crate) fn crop_png_to_file(
    input: &Path,
    crop: PageCrop,
    output: &Path,
) -> Result<(), ToolError> {
    let image = read_png(input)?;
    let left = (crop.left.max(0) as u32).min(image.width.saturating_sub(1));
    let top = (crop.top.max(0) as u32).min(image.height.saturating_sub(1));
    let right = left
        .saturating_add(crop.width.max(1) as u32)
        .min(image.width)
        .max(left + 1);
    let bottom = top
        .saturating_add(crop.height.max(1) as u32)
        .min(image.height)
        .max(top + 1);
    let width = right - left;
    let height = bottom - top;
    let row_stride = image.width as usize * image.bytes_per_pixel;
    let crop_stride = width as usize * image.bytes_per_pixel;
    let mut pixels = Vec::with_capacity(crop_stride * height as usize);
    for row in top as usize..bottom as usize {
        let start = row * row_stride + left as usize * image.bytes_per_pixel;
        pixels.extend_from_slice(&image.pixels[start..start + crop_stride]);
    }
    write_png(
        output,
        width,
        height,
        image.color_type,
        image.bit_depth,
        image.bytes_per_pixel,
        &pixels,
    )
}

#[cfg(test)]
pub(crate) fn write_solid_rgb_png(
    output: &Path,
    width: u32,
    height: u32,
    rgb: [u8; 3],
) -> Result<(), ToolError> {
    let mut pixels = Vec::with_capacity(width as usize * height as usize * 3);
    for _ in 0..width as usize * height as usize {
        pixels.extend_from_slice(&rgb);
    }
    write_png(output, width, height, 2, 8, 3, &pixels)
}

fn read_png(path: &Path) -> Result<PngImage, ToolError> {
    let bytes = fs::read(path)?;
    if bytes.len() < 8 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" {
        return Err(ToolError::UnsupportedPng(format!(
            "{} is not a PNG file",
            path.display()
        )));
    }

    let mut offset = 8;
    let mut width = None;
    let mut height = None;
    let mut bit_depth = None;
    let mut color_type = None;
    let mut idat = Vec::new();
    while offset + 8 <= bytes.len() {
        let length = u32::from_be_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .expect("slice length checked"),
        ) as usize;
        let kind = &bytes[offset + 4..offset + 8];
        let data_start = offset + 8;
        let data_end = data_start + length;
        if data_end + 4 > bytes.len() {
            return Err(ToolError::UnsupportedPng(format!(
                "{} has a truncated chunk",
                path.display()
            )));
        }
        let data = &bytes[data_start..data_end];
        match kind {
            b"IHDR" => {
                if data.len() != 13 {
                    return Err(ToolError::UnsupportedPng("invalid IHDR length".to_string()));
                }
                width = Some(u32::from_be_bytes(
                    data[0..4].try_into().expect("IHDR width"),
                ));
                height = Some(u32::from_be_bytes(
                    data[4..8].try_into().expect("IHDR height"),
                ));
                bit_depth = Some(data[8]);
                color_type = Some(data[9]);
                if data[10] != 0 || data[11] != 0 || data[12] != 0 {
                    return Err(ToolError::UnsupportedPng(
                        "compressed, filtered, or interlaced variant is unsupported".to_string(),
                    ));
                }
            }
            b"IDAT" => idat.extend_from_slice(data),
            b"IEND" => break,
            _ => {}
        }
        offset = data_end + 4;
    }

    let width = width.ok_or_else(|| ToolError::UnsupportedPng("missing IHDR".to_string()))?;
    let height = height.ok_or_else(|| ToolError::UnsupportedPng("missing IHDR".to_string()))?;
    let bit_depth = bit_depth.unwrap_or_default();
    let color_type = color_type.unwrap_or_default();
    if bit_depth != 8 {
        return Err(ToolError::UnsupportedPng(format!(
            "bit depth {bit_depth} is unsupported"
        )));
    }
    let bytes_per_pixel = match color_type {
        0 => 1,
        2 => 3,
        4 => 2,
        6 => 4,
        other => {
            return Err(ToolError::UnsupportedPng(format!(
                "color type {other} is unsupported"
            )));
        }
    };
    let mut decoder = ZlibDecoder::new(idat.as_slice());
    let mut inflated = Vec::new();
    decoder.read_to_end(&mut inflated)?;
    let row_stride = width as usize * bytes_per_pixel;
    let expected = (row_stride + 1) * height as usize;
    if inflated.len() != expected {
        return Err(ToolError::UnsupportedPng(format!(
            "inflated data length {} did not match expected {expected}",
            inflated.len()
        )));
    }

    let mut pixels = vec![0; row_stride * height as usize];
    for row in 0..height as usize {
        let raw_offset = row * (row_stride + 1);
        let filter = inflated[raw_offset];
        let raw = &inflated[raw_offset + 1..raw_offset + 1 + row_stride];
        let out_offset = row * row_stride;
        for column in 0..row_stride {
            let a = if column >= bytes_per_pixel {
                pixels[out_offset + column - bytes_per_pixel]
            } else {
                0
            };
            let b = if row > 0 {
                pixels[out_offset + column - row_stride]
            } else {
                0
            };
            let c = if row > 0 && column >= bytes_per_pixel {
                pixels[out_offset + column - row_stride - bytes_per_pixel]
            } else {
                0
            };
            let predictor = match filter {
                0 => 0,
                1 => a,
                2 => b,
                3 => ((a as u16 + b as u16) / 2) as u8,
                4 => paeth(a, b, c),
                other => {
                    return Err(ToolError::UnsupportedPng(format!(
                        "filter type {other} is unsupported"
                    )));
                }
            };
            pixels[out_offset + column] = raw[column].wrapping_add(predictor);
        }
    }

    Ok(PngImage {
        width,
        height,
        color_type,
        bit_depth,
        bytes_per_pixel,
        pixels,
    })
}

fn write_png(
    path: &Path,
    width: u32,
    height: u32,
    color_type: u8,
    bit_depth: u8,
    bytes_per_pixel: usize,
    pixels: &[u8],
) -> Result<(), ToolError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let row_stride = width as usize * bytes_per_pixel;
    if pixels.len() != row_stride * height as usize {
        return Err(ToolError::UnsupportedPng(
            "pixel buffer length did not match PNG dimensions".to_string(),
        ));
    }
    let mut filtered = Vec::with_capacity((row_stride + 1) * height as usize);
    for row in 0..height as usize {
        filtered.push(0);
        let start = row * row_stride;
        filtered.extend_from_slice(&pixels[start..start + row_stride]);
    }
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&filtered)?;
    let compressed = encoder.finish()?;

    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.push(bit_depth);
    ihdr.push(color_type);
    ihdr.extend_from_slice(&[0, 0, 0]);
    write_chunk(&mut bytes, b"IHDR", &ihdr);
    write_chunk(&mut bytes, b"IDAT", &compressed);
    write_chunk(&mut bytes, b"IEND", &[]);
    fs::write(path, bytes)?;
    Ok(())
}

fn write_chunk(output: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    output.extend_from_slice(&(data.len() as u32).to_be_bytes());
    output.extend_from_slice(kind);
    output.extend_from_slice(data);
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(kind);
    hasher.update(data);
    output.extend_from_slice(&hasher.finalize().to_be_bytes());
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let a = a as i32;
    let b = b as i32;
    let c = c as i32;
    let p = a + b - c;
    let pa = (p - a).abs();
    let pb = (p - b).abs();
    let pc = (p - c).abs();
    if pa <= pb && pa <= pc {
        a as u8
    } else if pb <= pc {
        b as u8
    } else {
        c as u8
    }
}

#[derive(Debug, Clone)]
struct ListedImage {
    num: usize,
    page: u32,
    kind: String,
    width: Option<u32>,
    height: Option<u32>,
}

fn parse_pdfimages_list(output: &str) -> Vec<ListedImage> {
    output
        .lines()
        .filter_map(|line| {
            let columns = line.split_whitespace().collect::<Vec<_>>();
            let page = columns.first()?.parse::<u32>().ok()?;
            let num = columns.get(1)?.parse::<usize>().ok()?;
            let kind = columns.get(2)?.to_string();
            let width = columns.get(3).and_then(|value| value.parse::<u32>().ok());
            let height = columns.get(4).and_then(|value| value.parse::<u32>().ok());
            Some(ListedImage {
                num,
                page,
                kind,
                width,
                height,
            })
        })
        .collect()
}

fn pair_extracted_images_with_list_rows(
    paths: Vec<PathBuf>,
    listed: &[ListedImage],
) -> Result<Vec<ExtractedImage>, ToolError> {
    if paths.len() != listed.len() {
        return Err(ToolError::ImageListMismatch(format!(
            "{} emitted file(s), {} listed row(s)",
            paths.len(),
            listed.len()
        )));
    }

    let listed_by_num = listed
        .iter()
        .map(|image| (image.num, image))
        .collect::<HashMap<_, _>>();
    let mut images = Vec::new();
    for path in paths {
        let num = image_file_index(&path).ok_or_else(|| {
            ToolError::ImageListMismatch(format!(
                "could not parse image index from {}",
                path.display()
            ))
        })?;
        let listed = listed_by_num.get(&num).ok_or_else(|| {
            ToolError::ImageListMismatch(format!(
                "emitted file {} has no -list row for num {num}",
                path.display()
            ))
        })?;
        if listed.kind != "image" {
            continue;
        }
        let index = images.len();
        images.push(ExtractedImage {
            page: listed.page,
            index,
            width: listed.width,
            height: listed.height,
            extension: path
                .extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or("png")
                .to_ascii_lowercase(),
            path,
        });
    }
    Ok(images)
}

fn image_file_index(path: &Path) -> Option<usize> {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| stem.rsplit('-').next())
        .and_then(|index| index.parse::<usize>().ok())
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

pub(crate) fn scoped_temp_dir(prefix: &str) -> Result<PathBuf, ToolError> {
    for _ in 0..TEMP_DIR_CREATE_ATTEMPTS {
        let mut random = [0_u8; TEMP_DIR_RANDOM_BYTES];
        fill_secure_random(&mut random)?;
        let suffix = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let path = std::env::temp_dir().join(format!("{prefix}-{suffix}"));
        let builder = fs::DirBuilder::new();
        #[cfg(unix)]
        let builder = {
            use std::os::unix::fs::DirBuilderExt;

            let mut builder = builder;
            builder.mode(0o700);
            builder
        };
        match builder.create(&path) {
            Ok(()) => return Ok(path),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err.into()),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create a unique secure temporary directory",
    )
    .into())
}

#[cfg(unix)]
fn fill_secure_random(bytes: &mut [u8]) -> io::Result<()> {
    fs::File::open("/dev/urandom")?.read_exact(bytes)
}

#[cfg(windows)]
fn fill_secure_random(bytes: &mut [u8]) -> io::Result<()> {
    const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;

    #[link(name = "bcrypt")]
    unsafe extern "system" {
        fn BCryptGenRandom(
            algorithm: *mut std::ffi::c_void,
            buffer: *mut u8,
            buffer_len: u32,
            flags: u32,
        ) -> i32;
    }

    let buffer_len = u32::try_from(bytes.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "random buffer is too large"))?;
    // SAFETY: `bytes` is writable for `buffer_len` bytes, the null algorithm
    // handle is required with BCRYPT_USE_SYSTEM_PREFERRED_RNG, and BCrypt does
    // not retain either pointer after returning.
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            bytes.as_mut_ptr(),
            buffer_len,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status >= 0 {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "BCryptGenRandom failed with NTSTATUS 0x{:08x}",
            status as u32
        )))
    }
}

#[cfg(not(any(unix, windows)))]
fn fill_secure_random(_bytes: &mut [u8]) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "secure temporary directories are unsupported on this platform",
    ))
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
    use std::ffi::OsStr;

    fn fake_tool_command(test_name: &str) -> Command {
        let executable = std::env::current_exe().expect("current test executable");
        let mut command = poppler_command(&executable);
        command.args(["--ignored", "--exact", test_name, "--nocapture"]);
        command
    }

    #[test]
    #[ignore = "stand-in executable invoked by timeout coverage"]
    fn fake_tool_sleeps_then_marks_completion() {
        thread::sleep(Duration::from_millis(750));
        fs::write("fake-tool-completed", b"completed").expect("completion marker writes");
    }

    #[test]
    #[ignore = "stand-in executable invoked by output-limit coverage"]
    fn fake_tool_emits_large_stdout() {
        let bytes = vec![b'x'; 4 * 1024];
        io::stdout().write_all(&bytes).expect("fake stdout writes");
        io::stdout().flush().expect("fake stdout flushes");
    }

    #[test]
    #[ignore = "stand-in parent process invoked by environment coverage"]
    fn fake_parent_builds_sanitized_poppler_command() {
        assert_eq!(
            std::env::var_os("DEEPSEEK_API_KEY").as_deref(),
            Some(OsStr::new("fixture-secret"))
        );
        let executable = find_tool("pdftohtml").expect("POPPLER_PATH discovery");
        let command = poppler_command(&executable);
        let environment = command.get_envs().collect::<Vec<_>>();

        assert!(environment.iter().all(|(name, _)| {
            !name
                .to_string_lossy()
                .eq_ignore_ascii_case("DEEPSEEK_API_KEY")
        }));
        assert!(
            environment
                .iter()
                .all(|(name, _)| { !name.to_string_lossy().eq_ignore_ascii_case("POPPLER_PATH") })
        );
        if std::env::var_os("PATH").is_some() {
            assert!(environment.iter().any(|(name, value)| {
                name.to_string_lossy().eq_ignore_ascii_case("PATH") && value.is_some()
            }));
        }
    }

    #[test]
    fn timed_out_child_is_killed_and_reaped() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut command = fake_tool_command("tools::tests::fake_tool_sleeps_then_marks_completion");
        command.current_dir(dir.path());

        let started = Instant::now();
        let error = command_output_with_limits(
            &mut command,
            "fake-poppler",
            Duration::from_millis(150),
            1024,
            1024,
        )
        .expect_err("the fake tool should time out");
        let elapsed = started.elapsed();

        assert_eq!(error.to_string(), "'fake-poppler' timed out after 150ms");
        assert!(matches!(
            &error,
            ToolError::TimedOut {
                tool: "fake-poppler",
                timeout
            } if *timeout == Duration::from_millis(150)
        ));
        assert!(elapsed < Duration::from_millis(700));
        thread::sleep(Duration::from_secs(1));
        assert!(
            !dir.path().join("fake-tool-completed").exists(),
            "completion marker proves the timed-out child survived"
        );
    }

    #[test]
    fn oversized_stdout_is_rejected_at_the_configured_limit() {
        let mut command = fake_tool_command("tools::tests::fake_tool_emits_large_stdout");
        let error = command_output_with_limits(
            &mut command,
            "fake-poppler",
            Duration::from_secs(5),
            1024,
            1024,
        )
        .expect_err("the fake tool should exceed stdout's limit");

        assert!(matches!(
            error,
            ToolError::OutputTooLarge {
                tool: "fake-poppler",
                stream: "stdout",
                limit: 1024
            }
        ));
    }

    #[test]
    fn poppler_command_keeps_discovery_working_without_inheriting_secrets() {
        let dir = tempfile::tempdir().expect("temp dir");
        let executable_name = if cfg!(windows) {
            "pdftohtml.exe"
        } else {
            "pdftohtml"
        };
        let executable = dir.path().join(executable_name);
        fs::write(&executable, b"fixture").expect("fake executable writes");
        let test_executable = std::env::current_exe().expect("current test executable");
        let output = Command::new(test_executable)
            .args([
                "--ignored",
                "--exact",
                "tools::tests::fake_parent_builds_sanitized_poppler_command",
            ])
            .env("POPPLER_PATH", dir.path())
            .env("DEEPSEEK_API_KEY", "fixture-secret")
            .output()
            .expect("environment stand-in runs");

        assert!(
            output.status.success(),
            "environment stand-in failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn scoped_temp_dirs_are_securely_randomized() {
        let first = scoped_temp_dir("bookforge-secure-temp").expect("first temp dir");
        let second = scoped_temp_dir("bookforge-secure-temp").expect("second temp dir");

        assert_ne!(first, second);
        assert!(first.is_dir());
        assert!(second.is_dir());
        fs::remove_dir_all(first).expect("first temp dir removes");
        fs::remove_dir_all(second).expect("second temp dir removes");
    }

    #[test]
    fn parses_pdfimages_list_rows() {
        let rows = parse_pdfimages_list(
            r#"page   num  type   width height color comp bpc  enc interp  object ID x-ppi y-ppi size ratio
--------------------------------------------------------------------------------------------
   1     0 image     640   480  rgb     3   8  image  no        12  0    72    72  12K 2.0%
   3     1 smask     120    80  gray    1   8  image  no        13  0    72    72  1K  1.0%
"#,
        );

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].page, 1);
        assert_eq!(rows[0].num, 0);
        assert_eq!(rows[0].kind, "image");
        assert_eq!(rows[0].width, Some(640));
        assert_eq!(rows[1].page, 3);
        assert_eq!(rows[1].num, 1);
        assert_eq!(rows[1].kind, "smask");
    }

    #[test]
    fn pairs_pdfimages_files_by_num_before_discarding_masks() {
        let dir = tempfile::tempdir().expect("temp dir");
        let image = dir.path().join("image-000-000.png");
        let mask = dir.path().join("image-000-001.png");
        let second = dir.path().join("image-000-002.png");
        let rows = vec![
            ListedImage {
                num: 0,
                page: 1,
                kind: "image".to_string(),
                width: Some(120),
                height: Some(80),
            },
            ListedImage {
                num: 1,
                page: 1,
                kind: "smask".to_string(),
                width: Some(120),
                height: Some(80),
            },
            ListedImage {
                num: 2,
                page: 4,
                kind: "image".to_string(),
                width: Some(300),
                height: Some(100),
            },
        ];

        let paired =
            pair_extracted_images_with_list_rows(vec![image, mask, second], &rows).expect("paired");

        assert_eq!(paired.len(), 2);
        assert_eq!(paired[0].page, 1);
        assert_eq!(paired[0].width, Some(120));
        assert_eq!(paired[1].page, 4);
        assert_eq!(paired[1].width, Some(300));
    }

    #[test]
    fn scales_pdftohtml_xml_crop_units_to_render_pixels() {
        let crop = PageCrop {
            page: 2,
            left: 72,
            top: 108,
            width: 216,
            height: 54,
        }
        .to_render_pixels();

        assert_eq!(crop.left, 100);
        assert_eq!(crop.top, 150);
        assert_eq!(crop.width, 300);
        assert_eq!(crop.height, 75);
    }

    #[test]
    fn crops_png_pixels_locally() {
        let dir = tempfile::tempdir().expect("temp dir");
        let full = dir.path().join("full.png");
        let cropped = dir.path().join("cropped.png");
        write_solid_rgb_png(&full, 20, 20, [12, 34, 56]).expect("png writes");

        crop_png_to_file(
            &full,
            PageCrop {
                page: 1,
                left: 5,
                top: 6,
                width: 7,
                height: 8,
            },
            &cropped,
        )
        .expect("crop writes");

        let image = read_png(&cropped).expect("cropped png reads");
        assert_eq!(image.width, 7);
        assert_eq!(image.height, 8);
        assert_eq!(image.pixels[..3], [12, 34, 56]);
    }
}
