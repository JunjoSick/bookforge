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
    process::{Command, Output},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use flate2::{Compression, read::ZlibDecoder, write::ZlibEncoder};

pub const PDF_RENDER_DPI: u32 = 150;
const PDF_XML_ZOOM: &str = "1.5";
const PDF_XML_ZOOM_NUM: i64 = 3;
const PDF_XML_ZOOM_DEN: i64 = 2;
const PDF_POINTS_PER_INCH: i64 = 72;

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
        let mut command = Command::new(&self.pdftohtml);
        command.arg("-v");
        let output = command_output(&mut command).ok()?;
        let banner = String::from_utf8_lossy(&output.stderr);
        banner.lines().next().map(|line| line.trim().to_string())
    }

    /// Run `pdftohtml -xml` and return the XML document.
    pub fn pdf_to_xml(&self, pdf: &Path) -> Result<String, ToolError> {
        let work_dir = scoped_temp_dir("bookforge-pdftohtml")?;
        let pdf = absolute_path(pdf)?;
        let mut command = Command::new(&self.pdftohtml);
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
        let output = command_output(&mut command)?;
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
        let mut command = Command::new(&self.pdftotext);
        command.args(["-enc", "UTF-8", "-q"]).arg(pdf).arg("-");
        let output = command_output(&mut command)?;
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
        let mut command = Command::new(self.pdfimages_path()?);
        command
            .args(["-png", "-p", "-print-filenames", "-q"])
            .arg(pdf)
            .arg(&root);
        let output = command_output(&mut command)?;
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
        fs::create_dir_all(output_dir)?;
        let root = output_dir.join(format!("page-{page:04}"));
        let page_arg = page.to_string();
        let dpi_arg = PDF_RENDER_DPI.to_string();
        let mut command = Command::new(self.pdftoppm_path()?);
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
        let output = command_output(&mut command)?;
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
        fs::create_dir_all(output_dir)?;
        let root = output_dir.join(name);
        let crop = crop.to_render_pixels();
        let page_arg = crop.page.to_string();
        let left_arg = crop.left.to_string();
        let top_arg = crop.top.to_string();
        let width_arg = crop.width.to_string();
        let height_arg = crop.height.to_string();
        let dpi_arg = PDF_RENDER_DPI.to_string();
        let mut command = Command::new(self.pdftoppm_path()?);
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
        let output = command_output(&mut command)?;
        if !output.status.success() {
            return Err(ToolError::Failed {
                tool: "pdftoppm",
                code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        Ok(root.with_extension("png"))
    }

    fn list_images(&self, pdf: &Path) -> Result<Vec<ListedImage>, ToolError> {
        let mut command = Command::new(self.pdfimages_path()?);
        command.args(["-list"]).arg(pdf);
        let output = command_output(&mut command)?;
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

fn command_output(command: &mut Command) -> io::Result<Output> {
    let mut delay = Duration::from_millis(5);
    for attempt in 0..4 {
        match command.output() {
            Ok(output) => return Ok(output),
            Err(err) if err.raw_os_error() == Some(26) && attempt < 3 => {
                thread::sleep(delay);
                delay *= 2;
            }
            Err(err) => return Err(err),
        }
    }
    command.output()
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
