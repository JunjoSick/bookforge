use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::Read,
    path::Path,
};

use bookforge_core::{
    BookforgeError, Result,
    ir::{
        Block, BlockId, BlockKind, Book, BookFormat, BookId, DomPath, InlineMark, Metadata,
        ProtectedSpan, ProtectedSpanKind, Resource, Section, SectionId, SpineItem, TextRun,
    },
    marker::{is_marker_token, strip_marker_tokens},
};
use quick_xml::{
    Reader,
    events::{BytesStart, Event},
};
use zip::ZipArchive;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpubInspection {
    pub title: Option<String>,
    pub spine_count: usize,
    pub manifest_count: usize,
    pub xhtml_count: usize,
    pub has_nav: bool,
    pub has_toc: bool,
    pub resource_count: usize,
    pub package_path: String,
    pub xhtml_spine_count: usize,
}

/// How much of the document's visible text the reader actually captures
/// into translatable blocks. Text that lives outside the recognized block
/// elements (for example directly inside `<div>`) is parsed over but never
/// extracted, and ships untranslated; this metric makes that visible
/// before any tokens are spent.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TextCoverage {
    pub total_chars: usize,
    pub captured_chars: usize,
    pub files: Vec<FileTextCoverage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTextCoverage {
    pub href: String,
    pub total_chars: usize,
    pub captured_chars: usize,
}

impl TextCoverage {
    pub fn percent(&self) -> f64 {
        coverage_percent(self.captured_chars, self.total_chars)
    }
}

impl FileTextCoverage {
    pub fn percent(&self) -> f64 {
        coverage_percent(self.captured_chars, self.total_chars)
    }

    pub fn uncaptured_chars(&self) -> usize {
        self.total_chars.saturating_sub(self.captured_chars)
    }
}

fn coverage_percent(captured: usize, total: usize) -> f64 {
    if total == 0 {
        100.0
    } else {
        (captured.min(total) as f64 / total as f64) * 100.0
    }
}

#[derive(Debug, Clone)]
struct PackageDocument {
    metadata: Metadata,
    manifest: Vec<Resource>,
    spine: Vec<SpineItem>,
    toc_id: Option<String>,
}

pub fn read_epub(path: &Path) -> Result<Book> {
    let mut archive = open_archive(path)?;
    validate_mimetype(&mut archive)?;
    let package_path = locate_package(&mut archive)?;
    let package_xml = read_archive_text(&mut archive, &package_path)?;
    let mut package = parse_package(&package_xml)?;
    let package_dir = package_base_dir(&package_path);
    let manifest_by_id = package
        .manifest
        .iter()
        .map(|item| (item.id.as_str(), item))
        .collect::<HashMap<_, _>>();
    let mut sections = Vec::new();
    let mut blocks = Vec::new();

    let package_section_id = SectionId("sec_metadata_opf".to_string());
    let mut package_blocks =
        extract_package_title_blocks(&package_xml, &package_section_id, blocks.len())?;
    if !package_blocks.is_empty() {
        let block_ids = package_blocks
            .iter()
            .map(|block| block.id.clone())
            .collect::<Vec<_>>();
        sections.push(Section {
            id: package_section_id,
            href: package_path.clone(),
            spine_index: 0,
            title: Some("OPF metadata".to_string()),
            heading_level: None,
            block_ids,
            prev: None,
            next: None,
        });
        blocks.append(&mut package_blocks);
    }

    for (toc_index, resource) in package
        .manifest
        .iter()
        .filter(|item| item.media_type == "application/x-dtbncx+xml")
        .enumerate()
    {
        let href = join_epub_path(&package_dir, &resource.href);
        let ncx = read_archive_text(&mut archive, &href)?;
        let section_id = SectionId(format!("sec_toc_{toc_index:06}"));
        let mut toc_blocks = extract_ncx_text_blocks(&ncx, &section_id, blocks.len())?;
        if toc_blocks.is_empty() {
            continue;
        }
        let block_ids = toc_blocks
            .iter()
            .map(|block| block.id.clone())
            .collect::<Vec<_>>();
        sections.push(Section {
            id: section_id,
            href,
            spine_index: 0,
            title: Some("NCX table of contents".to_string()),
            heading_level: None,
            block_ids,
            prev: None,
            next: None,
        });
        blocks.append(&mut toc_blocks);
    }

    let spine_idrefs = package
        .spine
        .iter()
        .map(|item| item.idref.clone())
        .collect::<HashSet<_>>();

    for (spine_index, spine_item) in package.spine.iter_mut().enumerate() {
        let Some(resource) = manifest_by_id.get(spine_item.idref.as_str()) else {
            return Err(BookforgeError::InvalidInput(format!(
                "spine item references missing manifest id '{}'",
                spine_item.idref
            )));
        };

        let href = join_epub_path(&package_dir, &resource.href);
        spine_item.href = Some(href.clone());

        if !is_xhtml_media_type(&resource.media_type) {
            continue;
        }

        let xhtml = read_archive_text(&mut archive, &href)?;
        let section_id = SectionId(format!("sec_{spine_index:06}"));
        let mut section_blocks = extract_blocks(&xhtml, &href, &section_id, blocks.len())?;
        if section_blocks.is_empty() {
            continue;
        }
        let block_ids = section_blocks
            .iter()
            .map(|block| block.id.clone())
            .collect::<Vec<_>>();
        let (title, heading_level) = first_heading(&section_blocks);

        sections.push(Section {
            id: section_id,
            href,
            spine_index,
            title,
            heading_level,
            block_ids,
            prev: None,
            next: None,
        });
        blocks.append(&mut section_blocks);
    }

    let spine_len = package.spine.len();
    for (nav_index, resource) in package
        .manifest
        .iter()
        .filter(|item| is_nav_item(item) && !spine_idrefs.contains(&item.id))
        .enumerate()
    {
        let href = join_epub_path(&package_dir, &resource.href);
        let xhtml = read_archive_text(&mut archive, &href)?;
        let section_id = SectionId(format!("sec_nav_{nav_index:06}"));
        let mut nav_blocks = extract_blocks(&xhtml, &href, &section_id, blocks.len())?;
        if nav_blocks.is_empty() {
            continue;
        }
        let block_ids = nav_blocks
            .iter()
            .map(|block| block.id.clone())
            .collect::<Vec<_>>();
        sections.push(Section {
            id: section_id,
            href,
            spine_index: spine_len + nav_index,
            title: Some("EPUB navigation".to_string()),
            heading_level: None,
            block_ids,
            prev: None,
            next: None,
        });
        blocks.append(&mut nav_blocks);
    }

    link_sections(&mut sections);

    if blocks.is_empty() {
        return Err(BookforgeError::InvalidInput(
            "EPUB contains no translatable blocks".to_string(),
        ));
    }

    Ok(Book {
        source_path: Some(path.to_path_buf()),
        id: BookId(package_path),
        format: BookFormat::Epub,
        metadata: package.metadata,
        manifest: package.manifest,
        spine: package.spine,
        sections,
        blocks,
    })
}

pub fn inspect_epub(path: &Path) -> Result<EpubInspection> {
    let mut archive = open_archive(path)?;
    validate_mimetype(&mut archive)?;

    let package_path = locate_package(&mut archive)?;
    let package_xml = read_archive_text(&mut archive, &package_path)?;
    let package = parse_package(&package_xml)?;
    let manifest_by_id = package
        .manifest
        .iter()
        .map(|item| (item.id.as_str(), item))
        .collect::<HashMap<_, _>>();

    let package_dir = package_base_dir(&package_path);
    let xhtml_count = package
        .manifest
        .iter()
        .filter(|item| is_xhtml_media_type(&item.media_type))
        .count();
    let has_nav = package.manifest.iter().any(is_nav_item);
    let has_toc = package
        .toc_id
        .as_deref()
        .and_then(|toc_id| manifest_by_id.get(toc_id))
        .is_some_and(|item| item.media_type == "application/x-dtbncx+xml")
        || package
            .manifest
            .iter()
            .any(|item| item.media_type == "application/x-dtbncx+xml");

    let mut xhtml_spine_count = 0;
    for item in &package.spine {
        let Some(resource) = manifest_by_id.get(item.idref.as_str()) else {
            return Err(BookforgeError::InvalidInput(format!(
                "spine item references missing manifest id '{}'",
                item.idref
            )));
        };

        if is_xhtml_media_type(&resource.media_type) {
            let href = join_epub_path(&package_dir, &resource.href);
            read_archive_text(&mut archive, &href)?;
            xhtml_spine_count += 1;
        }
    }

    Ok(EpubInspection {
        title: package.metadata.title,
        spine_count: package.spine.len(),
        manifest_count: package.manifest.len(),
        xhtml_count,
        has_nav,
        has_toc,
        resource_count: package
            .manifest
            .iter()
            .filter(|item| !is_xhtml_media_type(&item.media_type))
            .count(),
        package_path,
        xhtml_spine_count,
    })
}

/// Measure how much visible body text each XHTML spine document contributes
/// versus how much the block extractor captures. Counts non-whitespace
/// characters so block boundaries and indentation do not skew the ratio.
pub fn text_coverage(path: &Path) -> Result<TextCoverage> {
    let mut archive = open_archive(path)?;
    validate_mimetype(&mut archive)?;
    let package_path = locate_package(&mut archive)?;
    let package_xml = read_archive_text(&mut archive, &package_path)?;
    let package = parse_package(&package_xml)?;
    let package_dir = package_base_dir(&package_path);
    let manifest_by_id = package
        .manifest
        .iter()
        .map(|item| (item.id.as_str(), item))
        .collect::<HashMap<_, _>>();

    let mut coverage = TextCoverage::default();
    for (spine_index, spine_item) in package.spine.iter().enumerate() {
        let Some(resource) = manifest_by_id.get(spine_item.idref.as_str()) else {
            return Err(BookforgeError::InvalidInput(format!(
                "spine item references missing manifest id '{}'",
                spine_item.idref
            )));
        };
        if !is_xhtml_media_type(&resource.media_type) {
            continue;
        }

        let href = join_epub_path(&package_dir, &resource.href);
        let xhtml = read_archive_text(&mut archive, &href)?;
        let section_id = SectionId(format!("sec_{spine_index:06}"));
        let blocks = extract_blocks(&xhtml, &href, &section_id, 0)?;
        let captured_chars = blocks
            .iter()
            .map(|block| non_whitespace_chars(&block_visible_text(block)))
            .sum::<usize>();
        let total_chars = visible_body_chars(&xhtml)?;

        coverage.total_chars += total_chars;
        coverage.captured_chars += captured_chars;
        coverage.files.push(FileTextCoverage {
            href,
            total_chars,
            captured_chars,
        });
    }

    Ok(coverage)
}

/// Non-whitespace character count of all reader-visible text: everything
/// inside `<body>` (minus `<script>`/`<style>` content) plus the document
/// `<title>`, which the extractor also captures and translates. Keeping
/// the numerator and denominator in sync stops per-file coverage from
/// exceeding 100% on title-bearing chapters.
fn visible_body_chars(xhtml: &str) -> Result<usize> {
    let mut reader = Reader::from_str(xhtml);
    reader.config_mut().trim_text(false);
    let mut in_body = false;
    let mut in_title = false;
    let mut skip_depth = 0usize;
    let mut count = 0usize;

    loop {
        let counting = (in_body || in_title) && skip_depth == 0;
        match reader.read_event()? {
            Event::Start(element) => match local_name(element.name().as_ref()) {
                b"body" => in_body = true,
                b"title" if !in_body => in_title = true,
                b"script" | b"style" if in_body => skip_depth += 1,
                _ => {}
            },
            Event::End(element) => match local_name(element.name().as_ref()) {
                b"body" => in_body = false,
                b"title" => in_title = false,
                b"script" | b"style" if skip_depth > 0 => skip_depth -= 1,
                _ => {}
            },
            Event::Text(text) if counting => {
                let value = text
                    .html_content()
                    .map_err(|err| BookforgeError::InvalidInput(err.to_string()))?;
                count += non_whitespace_chars(&value);
            }
            Event::CData(text) if counting => {
                let value = text
                    .decode()
                    .map_err(|err| BookforgeError::InvalidInput(err.to_string()))?;
                count += non_whitespace_chars(&value);
            }
            Event::GeneralRef(reference) if counting => {
                if let Some(value) = resolve_general_ref(&reference)? {
                    count += non_whitespace_chars(&value);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(count)
}

fn non_whitespace_chars(text: &str) -> usize {
    text.chars().filter(|ch| !ch.is_whitespace()).count()
}

fn open_archive(path: &Path) -> Result<ZipArchive<File>> {
    let file = File::open(path)?;
    Ok(ZipArchive::new(file)?)
}

fn validate_mimetype(archive: &mut ZipArchive<File>) -> Result<()> {
    let mut mimetype = String::new();
    archive.by_name("mimetype")?.read_to_string(&mut mimetype)?;

    if mimetype.trim() != "application/epub+zip" {
        return Err(BookforgeError::InvalidInput(
            "EPUB mimetype must be application/epub+zip".to_string(),
        ));
    }

    Ok(())
}

fn locate_package(archive: &mut ZipArchive<File>) -> Result<String> {
    let container = read_archive_text(archive, "META-INF/container.xml")?;
    let mut reader = Reader::from_str(&container);
    reader.config_mut().trim_text(true);

    loop {
        match reader.read_event()? {
            Event::Empty(element) | Event::Start(element)
                if local_name(element.name().as_ref()) == b"rootfile" =>
            {
                if let Some(path) = attr_value(&reader, &element, b"full-path")? {
                    return Ok(path);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Err(BookforgeError::InvalidInput(
        "META-INF/container.xml does not contain a rootfile full-path".to_string(),
    ))
}

fn parse_package(xml: &str) -> Result<PackageDocument> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut metadata = Metadata::default();
    let mut manifest = Vec::new();
    let mut spine = Vec::new();
    let mut toc_id = None;
    let mut current_text_element: Option<Vec<u8>> = None;

    loop {
        match reader.read_event()? {
            Event::Start(element) => match local_name(element.name().as_ref()) {
                b"title" | b"creator" | b"language" => {
                    current_text_element = Some(local_name(element.name().as_ref()).to_vec());
                }
                b"item" => manifest.push(parse_manifest_item(&reader, &element)?),
                b"spine" => {
                    toc_id = attr_value(&reader, &element, b"toc")?;
                }
                b"itemref" => {
                    spine.push(parse_spine_item(&reader, &element)?);
                }
                _ => {}
            },
            Event::Empty(element) => match local_name(element.name().as_ref()) {
                b"item" => manifest.push(parse_manifest_item(&reader, &element)?),
                b"itemref" => spine.push(parse_spine_item(&reader, &element)?),
                _ => {}
            },
            Event::Text(text) => {
                if let Some(name) = current_text_element.as_deref() {
                    let value = text
                        .html_content()
                        .map_err(|err| BookforgeError::InvalidInput(err.to_string()))?
                        .trim()
                        .to_string();
                    if !value.is_empty() {
                        match name {
                            b"title" if metadata.title.is_none() => metadata.title = Some(value),
                            b"creator" => metadata.creators.push(value),
                            b"language" if metadata.language.is_none() => {
                                metadata.language = Some(value)
                            }
                            _ => {}
                        }
                    }
                }
            }
            Event::End(element)
                if current_text_element
                    .as_deref()
                    .is_some_and(|name| name == local_name(element.name().as_ref())) =>
            {
                current_text_element = None;
            }
            Event::Eof => break,
            _ => {}
        }
    }

    if manifest.is_empty() {
        return Err(BookforgeError::InvalidInput(
            "OPF manifest is empty".to_string(),
        ));
    }

    if spine.is_empty() {
        return Err(BookforgeError::InvalidInput(
            "OPF spine is empty".to_string(),
        ));
    }

    Ok(PackageDocument {
        metadata,
        manifest,
        spine,
        toc_id,
    })
}

fn parse_manifest_item(reader: &Reader<&[u8]>, element: &BytesStart<'_>) -> Result<Resource> {
    let id = required_attr(reader, element, b"id", "manifest item id")?;
    let href = required_attr(reader, element, b"href", "manifest item href")?;
    let media_type = required_attr(reader, element, b"media-type", "manifest item media-type")?;

    Ok(Resource {
        id,
        href,
        media_type,
        properties: attr_value(reader, element, b"properties")?
            .map(|value| {
                value
                    .split_ascii_whitespace()
                    .map(ToOwned::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
    })
}

fn parse_spine_item(reader: &Reader<&[u8]>, element: &BytesStart<'_>) -> Result<SpineItem> {
    let idref = required_attr(reader, element, b"idref", "spine item idref")?;
    let linear = attr_value(reader, element, b"linear")?.is_none_or(|value| value != "no");

    Ok(SpineItem {
        idref,
        href: None,
        linear,
    })
}

fn required_attr(
    reader: &Reader<&[u8]>,
    element: &BytesStart<'_>,
    attr_name: &[u8],
    label: &str,
) -> Result<String> {
    attr_value(reader, element, attr_name)?.ok_or_else(|| {
        BookforgeError::InvalidInput(format!(
            "missing required {label} attribute '{}'",
            String::from_utf8_lossy(attr_name)
        ))
    })
}

fn attr_value(
    reader: &Reader<&[u8]>,
    element: &BytesStart<'_>,
    attr_name: &[u8],
) -> Result<Option<String>> {
    for attr in element.attributes() {
        let attr = attr.map_err(|err| BookforgeError::InvalidInput(err.to_string()))?;
        if local_name(attr.key.as_ref()) == attr_name {
            return Ok(Some(
                attr.decode_and_unescape_value(reader.decoder())?
                    .into_owned(),
            ));
        }
    }

    Ok(None)
}

#[derive(Debug)]
struct ElementFrame {
    name: Vec<u8>,
    path: Vec<usize>,
    child_count: usize,
    text_count: usize,
}

struct TextCapture {
    depth: usize,
    path: Vec<usize>,
    text: String,
}

fn extract_package_title_blocks(
    xml: &str,
    section_id: &SectionId,
    initial_block_count: usize,
) -> Result<Vec<Block>> {
    extract_xml_text_element_blocks(xml, section_id, initial_block_count, |name| {
        name == b"title"
    })
}

fn extract_ncx_text_blocks(
    xml: &str,
    section_id: &SectionId,
    initial_block_count: usize,
) -> Result<Vec<Block>> {
    extract_xml_text_element_blocks(xml, section_id, initial_block_count, |name| name == b"text")
}

fn extract_xml_text_element_blocks(
    xml: &str,
    section_id: &SectionId,
    initial_block_count: usize,
    should_capture: impl Fn(&[u8]) -> bool,
) -> Result<Vec<Block>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);

    let mut element_stack = Vec::<ElementFrame>::new();
    let mut active_capture: Option<TextCapture> = None;
    let mut blocks = Vec::new();

    loop {
        match reader.read_event()? {
            Event::Start(element) => {
                let name = local_name(element.name().as_ref()).to_vec();
                let path = enter_element(&mut element_stack, &name);
                if active_capture.is_none() && should_capture(&name) {
                    active_capture = Some(TextCapture {
                        depth: element_stack.len(),
                        path,
                        text: String::new(),
                    });
                }
            }
            Event::Empty(_) => {
                next_child_path(&mut element_stack);
            }
            Event::Text(text) => {
                if let Some(capture) = active_capture.as_mut() {
                    let value = text
                        .html_content()
                        .map_err(|err| BookforgeError::InvalidInput(err.to_string()))?;
                    capture.text.push_str(&value);
                }
            }
            Event::CData(text) => {
                if let Some(capture) = active_capture.as_mut() {
                    let value = text
                        .decode()
                        .map_err(|err| BookforgeError::InvalidInput(err.to_string()))?;
                    capture.text.push_str(&value);
                }
            }
            Event::GeneralRef(reference) => {
                if let Some(capture) = active_capture.as_mut()
                    && let Some(value) = resolve_general_ref(&reference)?
                {
                    capture.text.push_str(&value);
                }
            }
            Event::End(_) => {
                if active_capture
                    .as_ref()
                    .is_some_and(|capture| element_stack.len() == capture.depth)
                {
                    let capture = active_capture.take().expect("checked above");
                    let visible = normalize_space(&capture.text);
                    if !visible.is_empty() {
                        blocks.push(build_block(
                            section_id,
                            initial_block_count + blocks.len(),
                            BlockKind::Paragraph,
                            DomPath(capture.path),
                            Vec::new(),
                            Vec::new(),
                            visible,
                        ));
                    }
                }
                element_stack.pop();
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(blocks)
}

#[derive(Debug)]
struct BlockBuilder {
    /// Stack depth of the element this block is anchored to. The block
    /// closes when an End event arrives while the stack is exactly this
    /// deep — name-independent, so nested same-name elements (li > ul >
    /// li, nested blockquotes) stay inside the block as inline markers
    /// instead of ending it early.
    anchor_depth: usize,
    kind: BlockKind,
    dom_path: DomPath,
    ordinal: usize,
    text_runs: Vec<TextRun>,
    inline_marks: Vec<InlineMark>,
    inline_stack: Vec<String>,
    visible_text: String,
    next_run: usize,
    next_marker: usize,
}

impl BlockBuilder {
    fn new(anchor_depth: usize, kind: BlockKind, dom_path: DomPath, ordinal: usize) -> Self {
        Self {
            anchor_depth,
            kind,
            dom_path,
            ordinal,
            text_runs: Vec::new(),
            inline_marks: Vec::new(),
            inline_stack: Vec::new(),
            visible_text: String::new(),
            next_run: 0,
            next_marker: 0,
        }
    }

    fn push_text(&mut self, text: &str) {
        let Some(mut text) = normalize_text_fragment(text) else {
            // Whitespace-only fragment (e.g. a resolved &nbsp; entity
            // reference): it still separates words. Keep it as a
            // standalone run so spaces between adjacent inline markers
            // stay between those markers instead of moving inside the
            // previous inline element.
            if !text.is_empty()
                && !self.visible_text.is_empty()
                && !self.visible_text.ends_with(' ')
            {
                self.visible_text.push(' ');
                self.push_run(" ".to_string());
            }
            return;
        };

        if self.visible_text.is_empty() {
            text = text.trim_start().to_string();
        }

        if text.is_empty() {
            return;
        }

        self.visible_text.push_str(&text);
        self.push_run(text);
    }

    fn push_inline_start(&mut self, name: &[u8]) {
        let id = marker_id("m", self.next_marker);
        self.next_marker += 1;
        self.inline_marks.push(InlineMark {
            id: id.clone(),
            kind: String::from_utf8_lossy(name).into_owned(),
        });
        self.inline_stack.push(id.clone());
        self.push_run(format!("<{id}>"));
    }

    fn push_inline_empty(&mut self, name: &[u8]) {
        let id = marker_id("r", self.next_marker);
        self.next_marker += 1;
        self.inline_marks.push(InlineMark {
            id: id.clone(),
            kind: String::from_utf8_lossy(name).into_owned(),
        });
        self.push_run(format!("<{id}/>"));
    }

    fn push_inline_end(&mut self) {
        if let Some(id) = self.inline_stack.pop() {
            self.push_run(format!("</{id}>"));
        }
    }

    fn finish(mut self, section_id: &SectionId) -> Option<Block> {
        self.trim_trailing_text();
        let visible_text = normalize_space(&self.visible_text);
        if visible_text.is_empty() {
            return None;
        }

        Some(build_block(
            section_id,
            self.ordinal,
            self.kind,
            self.dom_path,
            self.text_runs,
            self.inline_marks,
            visible_text,
        ))
    }

    fn push_run(&mut self, text: String) {
        self.text_runs.push(TextRun {
            id: format!("r{:06}_{:03}", self.ordinal, self.next_run),
            text,
        });
        self.next_run += 1;
    }

    fn trim_trailing_text(&mut self) {
        if let Some(run) = self
            .text_runs
            .iter_mut()
            .rev()
            .find(|run| !is_marker_token(&run.text))
        {
            run.text = run.text.trim_end().to_string();
        }

        self.text_runs.retain(|run| !run.text.is_empty());
    }
}

fn extract_blocks(
    xhtml: &str,
    _href: &str,
    section_id: &SectionId,
    initial_block_count: usize,
) -> Result<Vec<Block>> {
    let mut reader = Reader::from_str(xhtml);
    reader.config_mut().trim_text(false);

    let mut element_stack = Vec::<ElementFrame>::new();
    let mut active_block: Option<BlockBuilder> = None;
    let mut blocks = Vec::new();
    // Depth of never-translate ancestors (script/style/head/svg/...).
    // While positive, loose text is not captured into lazy blocks.
    let mut suppress_depth = 0usize;

    loop {
        match reader.read_event()? {
            Event::Start(element) => {
                let name = local_name(element.name().as_ref()).to_vec();
                let path = enter_element(&mut element_stack, &name);
                if never_translate_element(&name) {
                    suppress_depth += 1;
                }

                // EPUB navigation list items have a constrained content
                // model: the label must remain inside the child <a> or
                // <span>, followed optionally by a nested <ol>. Anchoring a
                // translation on the <li> itself can move translated text
                // before the link and produce EPUBCheck RSC-005 errors.
                let navigation_list_item = name == b"li"
                    && element_stack[..element_stack.len().saturating_sub(1)]
                        .iter()
                        .any(|frame| frame.name == b"nav");
                if active_block.is_none()
                    && !navigation_list_item
                    && let Some(kind) = block_kind(&name, &element)?
                {
                    active_block = Some(BlockBuilder::new(
                        element_stack.len(),
                        kind,
                        DomPath(path),
                        initial_block_count + blocks.len(),
                    ));
                } else if let Some(block) = active_block.as_mut() {
                    block.push_inline_start(&name);
                }
            }
            Event::Empty(element) => {
                let name = local_name(element.name().as_ref()).to_vec();
                // Sibling bookkeeping must advance even though self-closing
                // block elements (<p/>, <td/>) carry no text and therefore
                // produce no block — emitting one would send an empty
                // source to the model and invite hallucinated output.
                next_child_path(&mut element_stack);

                if let Some(block) = active_block.as_mut() {
                    block.push_inline_empty(&name);
                }
            }
            Event::Text(text) => {
                let value = text
                    .html_content()
                    .map_err(|err| BookforgeError::InvalidInput(err.to_string()))?;
                handle_text(
                    &value,
                    &mut active_block,
                    &mut element_stack,
                    &mut blocks,
                    section_id,
                    initial_block_count,
                    suppress_depth > 0,
                    true,
                );
            }
            Event::CData(text) => {
                let value = text
                    .decode()
                    .map_err(|err| BookforgeError::InvalidInput(err.to_string()))?;
                handle_text(
                    &value,
                    &mut active_block,
                    &mut element_stack,
                    &mut blocks,
                    section_id,
                    initial_block_count,
                    suppress_depth > 0,
                    true,
                );
            }
            // quick-xml surfaces entity references (&nbsp; &mdash; ...) as
            // separate events rather than resolving them inside Text.
            // Resolve numeric and HTML5 named references; the resolved
            // text joins the active block or may anchor a lazy one, but
            // never consumes a stray text-node index — the writer counts
            // Text events only, and indices must stay aligned.
            Event::GeneralRef(reference) => {
                if let Some(value) = resolve_general_ref(&reference)? {
                    handle_text(
                        &value,
                        &mut active_block,
                        &mut element_stack,
                        &mut blocks,
                        section_id,
                        initial_block_count,
                        suppress_depth > 0,
                        false,
                    );
                }
            }
            Event::End(_) => {
                let should_finish = active_block
                    .as_ref()
                    .is_some_and(|block| element_stack.len() == block.anchor_depth);

                if should_finish {
                    let block = active_block.take().expect("checked above");
                    if let Some(block) = block.finish(section_id) {
                        blocks.push(block);
                    }
                } else if let Some(block) = active_block.as_mut() {
                    block.push_inline_end();
                }

                if element_stack
                    .pop()
                    .is_some_and(|frame| never_translate_element(&frame.name))
                {
                    suppress_depth = suppress_depth.saturating_sub(1);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(blocks)
}

/// Resolve a general entity reference to its replacement text: numeric
/// character references and the HTML5 named set. Unknown entities are
/// dropped with a warning rather than failing the whole book.
fn resolve_general_ref(reference: &quick_xml::events::BytesRef<'_>) -> Result<Option<String>> {
    if let Some(ch) = reference
        .resolve_char_ref()
        .map_err(|err| BookforgeError::InvalidInput(err.to_string()))?
    {
        return Ok(Some(ch.to_string()));
    }
    let name = reference
        .decode()
        .map_err(|err| BookforgeError::InvalidInput(err.to_string()))?;
    let resolved = quick_xml::escape::resolve_html5_entity(&name).map(ToString::to_string);
    if resolved.is_none() {
        tracing::warn!(entity = %name, "dropping unresolvable entity reference");
    }
    Ok(resolved)
}

/// Route a decoded text fragment: into the active block if there is one,
/// otherwise — for non-whitespace text the block whitelist missed — start
/// a block anchored on the enclosing element (text-bearing `<div>`,
/// `<dt>`, `<dd>`, ...) or, when earlier element children make whole-
/// element patching unsafe, record a standalone text-node block the
/// writer can address directly. Without this fallback such text silently
/// shipped untranslated.
#[allow(clippy::too_many_arguments)]
fn handle_text(
    value: &str,
    active_block: &mut Option<BlockBuilder>,
    element_stack: &mut [ElementFrame],
    blocks: &mut Vec<Block>,
    section_id: &SectionId,
    initial_block_count: usize,
    suppressed: bool,
    allow_stray: bool,
) {
    if let Some(block) = active_block.as_mut() {
        block.push_text(value);
        return;
    }
    if suppressed || value.trim().is_empty() {
        return;
    }
    let depth = element_stack.len();
    let Some(frame) = element_stack.last_mut() else {
        return;
    };
    if frame.child_count == 0 && anchors_text_block(&frame.name) {
        let mut block = BlockBuilder::new(
            depth,
            BlockKind::Paragraph,
            DomPath(frame.path.clone()),
            initial_block_count + blocks.len(),
        );
        block.push_text(value);
        *active_block = Some(block);
        return;
    }
    if !allow_stray {
        return;
    }
    // Stray text node: prior element siblings (or a wrapper element)
    // make whole-element patching unsafe, so the text node itself
    // becomes the patch target. The writer counts non-whitespace text
    // nodes per frame with the same rule.
    let mut path = frame.path.clone();
    path.push(bookforge_core::ir::TEXT_NODE_PATH_BASE + frame.text_count);
    frame.text_count += 1;
    let visible = normalize_space(value);
    if visible.is_empty() {
        return;
    }
    blocks.push(build_block(
        section_id,
        initial_block_count + blocks.len(),
        BlockKind::Paragraph,
        DomPath(path),
        Vec::new(),
        Vec::new(),
        visible,
    ));
}

/// Elements whose text must never be translated.
fn never_translate_element(name: &[u8]) -> bool {
    matches!(name, b"script" | b"style" | b"svg" | b"math")
}

/// Elements safe to anchor a lazily-started text block on. Structural
/// wrappers are excluded: anchoring on them would swallow every nested
/// block element into one giant marker-laden block. Their direct text is
/// handled as stray text nodes instead.
fn anchors_text_block(name: &[u8]) -> bool {
    !matches!(
        name,
        b"body"
            | b"html"
            | b"section"
            | b"article"
            | b"main"
            | b"nav"
            | b"head"
            | b"header"
            | b"footer"
            | b"aside"
            | b"figure"
            | b"ul"
            | b"ol"
            | b"dl"
            | b"table"
            | b"thead"
            | b"tbody"
            | b"tfoot"
            | b"colgroup"
    )
}

fn enter_element(stack: &mut Vec<ElementFrame>, name: &[u8]) -> Vec<usize> {
    let path = next_child_path(stack);
    stack.push(ElementFrame {
        name: name.to_vec(),
        path: path.clone(),
        child_count: 0,
        text_count: 0,
    });
    path
}

fn next_child_path(stack: &mut [ElementFrame]) -> Vec<usize> {
    let Some(parent) = stack.last_mut() else {
        return vec![0];
    };
    let child_index = parent.child_count;
    parent.child_count += 1;
    let mut path = parent.path.clone();
    path.push(child_index);
    path
}

fn block_kind(name: &[u8], element: &BytesStart<'_>) -> Result<Option<BlockKind>> {
    Ok(match name {
        b"h1" => Some(BlockKind::Heading(1)),
        b"h2" => Some(BlockKind::Heading(2)),
        b"h3" => Some(BlockKind::Heading(3)),
        b"h4" => Some(BlockKind::Heading(4)),
        b"h5" => Some(BlockKind::Heading(5)),
        b"h6" => Some(BlockKind::Heading(6)),
        b"p" => Some(BlockKind::Paragraph),
        b"li" => Some(BlockKind::ListItem),
        b"blockquote" => Some(BlockKind::Quote),
        b"td" | b"th" => Some(BlockKind::TableCell),
        b"tr" => Some(BlockKind::TableRow),
        b"figcaption" | b"caption" => Some(BlockKind::Caption),
        b"pre" | b"code" => Some(BlockKind::Code),
        b"aside" if has_epub_type(element, b"footnote")? => Some(BlockKind::Footnote),
        _ => None,
    })
}

fn has_epub_type(element: &BytesStart<'_>, expected: &[u8]) -> Result<bool> {
    for attr in element.attributes() {
        let attr = attr.map_err(|err| BookforgeError::InvalidInput(err.to_string()))?;
        if local_name(attr.key.as_ref()) == b"type" {
            let value = attr.unescape_value()?.into_owned();
            return Ok(value
                .split_ascii_whitespace()
                .any(|item| item.as_bytes() == expected));
        }
    }
    Ok(false)
}

fn build_block(
    section_id: &SectionId,
    ordinal: usize,
    kind: BlockKind,
    dom_path: DomPath,
    text_runs: Vec<TextRun>,
    inline_marks: Vec<InlineMark>,
    visible_text: String,
) -> Block {
    let text_runs = if text_runs.is_empty() {
        vec![TextRun {
            id: format!("r{ordinal:06}_000"),
            text: visible_text.clone(),
        }]
    } else {
        text_runs
    };
    let protected_spans = detect_protected_spans(&visible_text);

    Block {
        id: BlockId(format!("b_{ordinal:06}")),
        section_id: section_id.clone(),
        kind,
        dom_path,
        text_runs,
        inline_marks,
        protected_spans,
        token_estimate: estimate_tokens(&visible_text),
    }
}

fn first_heading(blocks: &[Block]) -> (Option<String>, Option<u8>) {
    blocks
        .iter()
        .find_map(|block| match block.kind {
            BlockKind::Heading(level) => Some((Some(block_visible_text(block)), Some(level))),
            _ => None,
        })
        .unwrap_or((None, None))
}

fn link_sections(sections: &mut [Section]) {
    let ids = sections
        .iter()
        .map(|section| section.id.clone())
        .collect::<Vec<_>>();

    for (index, section) in sections.iter_mut().enumerate() {
        section.prev = index.checked_sub(1).and_then(|prev| ids.get(prev).cloned());
        section.next = ids.get(index + 1).cloned();
    }
}

fn normalize_space(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn normalize_text_fragment(text: &str) -> Option<String> {
    if text.trim().is_empty() {
        return None;
    }

    let mut normalized = normalize_space(text);
    if text.chars().next().is_some_and(char::is_whitespace) {
        normalized.insert(0, ' ');
    }
    if text.chars().last().is_some_and(char::is_whitespace) {
        normalized.push(' ');
    }
    Some(normalized)
}

fn block_visible_text(block: &Block) -> String {
    let marked = block
        .text_runs
        .iter()
        .map(|run| run.text.as_str())
        .collect::<Vec<_>>()
        .join("");
    normalize_space(&strip_marker_tokens(&marked))
}

fn marker_id(prefix: &str, marker_ordinal: usize) -> String {
    format!("{prefix}{}", marker_ordinal + 1)
}

fn estimate_tokens(text: &str) -> usize {
    let words = text.split_whitespace().count();
    words.saturating_mul(4).div_ceil(3).max(1)
}

fn detect_protected_spans(text: &str) -> Vec<ProtectedSpan> {
    let mut spans = text
        .split_whitespace()
        .filter_map(|raw| {
            let value = trim_token(raw);
            protected_span_kind(value).map(|kind| ProtectedSpan {
                kind,
                text: value.to_string(),
            })
        })
        .collect::<Vec<_>>();
    spans.sort_by(|left, right| left.text.cmp(&right.text));
    spans.dedup_by(|left, right| left.kind == right.kind && left.text == right.text);
    spans
}

fn protected_span_kind(value: &str) -> Option<ProtectedSpanKind> {
    if value.is_empty() {
        None
    } else if value.starts_with("http://") || value.starts_with("https://") {
        Some(ProtectedSpanKind::Url)
    } else if value.starts_with('#') && value.len() > 1 {
        Some(ProtectedSpanKind::InternalAnchor)
    } else if looks_like_email(value) {
        Some(ProtectedSpanKind::Email)
    } else if looks_like_citation(value) {
        Some(ProtectedSpanKind::Citation)
    } else if looks_like_protected_number(value) {
        Some(ProtectedSpanKind::Number)
    } else if looks_like_filename(value) {
        Some(ProtectedSpanKind::Filename)
    } else {
        None
    }
}

fn trim_token(raw: &str) -> &str {
    let trimmed = raw.trim_matches(|ch: char| {
        matches!(
            ch,
            ',' | ';' | ':' | '.' | '!' | '?' | '(' | ')' | '"' | '\''
        )
    });
    if trimmed.starts_with("[@") && trimmed.ends_with(']') {
        trimmed
    } else {
        trimmed.trim_matches(|ch: char| matches!(ch, '[' | ']'))
    }
}

fn looks_like_email(value: &str) -> bool {
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.')
}

fn looks_like_citation(value: &str) -> bool {
    (value.starts_with('@') && value.len() > 1)
        || (value.starts_with("[@") && value.ends_with(']') && value.len() > 3)
}

fn looks_like_filename(value: &str) -> bool {
    let Some((stem, ext)) = value.rsplit_once('.') else {
        return false;
    };
    const COMMON_EXTENSIONS: &[&str] = &[
        "azw", "css", "csv", "epub", "gif", "htm", "html", "jpeg", "jpg", "js", "json", "md",
        "mobi", "ncx", "opf", "pdf", "png", "svg", "txt", "xhtml", "xml", "zip",
    ];
    let ext = ext.to_ascii_lowercase();
    !stem.is_empty()
        && COMMON_EXTENSIONS.contains(&ext.as_str())
        && stem
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '/' | '.'))
}

fn looks_like_protected_number(value: &str) -> bool {
    let digit_count = value.chars().filter(|ch| ch.is_ascii_digit()).count();
    if digit_count == 0 {
        return false;
    }
    if digit_count >= 2 {
        return value.chars().all(|ch| {
            ch.is_ascii_digit()
                || matches!(
                    ch,
                    '.' | ',' | ':' | ';' | '/' | '-' | '+' | '%' | '$' | '\u{20ac}' | '\u{00a3}'
                )
        });
    }
    value.ends_with("st") || value.ends_with("nd") || value.ends_with("rd") || value.ends_with("th")
}

fn read_archive_text(archive: &mut ZipArchive<File>, path: &str) -> Result<String> {
    let mut file = archive.by_name(path)?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(text)
}

fn is_xhtml_media_type(media_type: &str) -> bool {
    matches!(
        media_type,
        "application/xhtml+xml" | "text/html" | "application/xml"
    )
}

fn is_nav_item(item: &Resource) -> bool {
    item.media_type == "application/xhtml+xml"
        && (item.properties.iter().any(|property| property == "nav")
            || item.href.ends_with("nav.xhtml"))
}

fn package_base_dir(package_path: &str) -> String {
    Path::new(package_path)
        .parent()
        .and_then(Path::to_str)
        .unwrap_or("")
        .to_string()
}

fn join_epub_path(base: &str, href: &str) -> String {
    let href = href
        .split('#')
        .next()
        .unwrap_or(href)
        .split('?')
        .next()
        .unwrap_or(href);
    let href = percent_decode_epub_path(href);
    if base.is_empty() {
        normalize_epub_path(&href)
    } else {
        normalize_epub_path(&format!("{base}/{href}"))
    }
}

fn normalize_epub_path(path: &str) -> String {
    let mut normalized = Vec::new();
    for component in Path::new(path).components() {
        match component {
            std::path::Component::Normal(value) => {
                normalized.push(value.to_string_lossy().to_string());
            }
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {}
        }
    }
    normalized.join("/")
}

fn percent_decode_epub_path(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
        {
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded)
        .unwrap_or_else(|err| String::from_utf8_lossy(err.as_bytes()).to_string())
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn local_name(name: &[u8]) -> &[u8] {
    name.rsplit(|byte| *byte == b':').next().unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opf_manifest_items_can_use_start_end_tags() {
        let package = parse_package(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>Title</dc:title>
  </metadata>
  <manifest>
    <item id="ch1" href="Text/ch1.xhtml" media-type="application/xhtml+xml"></item>
  </manifest>
  <spine>
    <itemref idref="ch1"></itemref>
  </spine>
</package>"#,
        )
        .expect("package should parse");

        assert_eq!(package.manifest.len(), 1);
        assert_eq!(package.manifest[0].href, "Text/ch1.xhtml");
        assert_eq!(package.spine.len(), 1);
    }

    #[test]
    fn opf_href_paths_are_percent_decoded_and_normalized() {
        assert_eq!(
            join_epub_path("OPS", "Text/../Text/Chapter%201.xhtml#frag"),
            "OPS/Text/Chapter 1.xhtml"
        );
        assert_eq!(
            join_epub_path("OPS/package", "../Text/%E2%82%AC.xhtml"),
            "OPS/Text/\u{20ac}.xhtml"
        );
    }

    #[test]
    fn extracts_inline_marks_and_marker_text_runs() {
        let section_id = SectionId("sec_000000".to_string());
        let blocks = extract_blocks(
            "<html><body><p>Hello <em>world</em>!</p></body></html>",
            "chapter.xhtml",
            &section_id,
            0,
        )
        .expect("block extraction should succeed");

        assert_eq!(blocks.len(), 1);
        let text = block_text(&blocks[0]);
        assert_eq!(text, "Hello <m1>world</m1>!");
        assert_eq!(blocks[0].inline_marks.len(), 1);
        assert_eq!(blocks[0].inline_marks[0].id, "m1");
        assert_eq!(blocks[0].inline_marks[0].kind, "em");
        assert_eq!(blocks[0].token_estimate, estimate_tokens("Hello world!"));
    }

    #[test]
    fn keeps_inter_inline_whitespace_between_marker_runs() {
        let section_id = SectionId("sec_000000".to_string());
        let blocks = extract_blocks(
            "<html><body><p><span>a</span> <span>b</span></p></body></html>",
            "chapter.xhtml",
            &section_id,
            0,
        )
        .expect("block extraction should succeed");

        assert_eq!(blocks.len(), 1);
        assert_eq!(block_text(&blocks[0]), "<m1>a</m1> <m2>b</m2>");
        let runs = blocks[0]
            .text_runs
            .iter()
            .map(|run| run.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(runs, vec!["<m1>", "a", "</m1>", " ", "<m2>", "b", "</m2>"]);
    }

    #[test]
    fn keeps_nbsp_only_inline_boundary_between_marker_runs() {
        let section_id = SectionId("sec_000000".to_string());
        let blocks = extract_blocks(
            "<html><body><p><span>a</span>&nbsp;<span>b</span></p></body></html>",
            "chapter.xhtml",
            &section_id,
            0,
        )
        .expect("block extraction should succeed");

        assert_eq!(blocks.len(), 1);
        assert_eq!(block_text(&blocks[0]), "<m1>a</m1> <m2>b</m2>");
    }

    #[test]
    fn extracts_empty_inline_marker() {
        let section_id = SectionId("sec_000000".to_string());
        let blocks = extract_blocks(
            "<html><body><p>Line<br/>break</p></body></html>",
            "chapter.xhtml",
            &section_id,
            4,
        )
        .expect("block extraction should succeed");

        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].id.0, "b_000004");
        assert_eq!(block_text(&blocks[0]), "Line<r1/>break");
        assert_eq!(blocks[0].inline_marks[0].id, "r1");
        assert_eq!(blocks[0].inline_marks[0].kind, "br");
    }

    #[test]
    fn extracts_text_anchored_block_from_div() {
        let section_id = SectionId("sec_000000".to_string());
        let blocks = extract_blocks(
            "<html><body><div class=\"x\">Bare div text with <em>emphasis</em>.</div></body></html>",
            "chapter.xhtml",
            &section_id,
            0,
        )
        .expect("block extraction should succeed");

        assert_eq!(blocks.len(), 1);
        assert_eq!(
            block_text(&blocks[0]),
            "Bare div text with <m1>emphasis</m1>."
        );
        assert_eq!(blocks[0].kind, BlockKind::Paragraph);
    }

    #[test]
    fn extracts_dt_and_dd_text() {
        let section_id = SectionId("sec_000000".to_string());
        let blocks = extract_blocks(
            "<html><body><dl><dt>Term</dt><dd>Definition</dd></dl></body></html>",
            "chapter.xhtml",
            &section_id,
            0,
        )
        .expect("block extraction should succeed");

        let texts: Vec<String> = blocks.iter().map(block_text).collect();
        assert_eq!(texts, vec!["Term".to_string(), "Definition".to_string()]);
    }

    #[test]
    fn stray_text_after_children_becomes_addressable_block() {
        let section_id = SectionId("sec_000000".to_string());
        let blocks = extract_blocks(
            "<html><body><p>Captured</p>Naked tail text</body></html>",
            "chapter.xhtml",
            &section_id,
            0,
        )
        .expect("block extraction should succeed");

        assert_eq!(blocks.len(), 2);
        assert_eq!(block_text(&blocks[1]), "Naked tail text");
        let last = *blocks[1]
            .dom_path
            .0
            .last()
            .expect("path should not be empty");
        assert!(
            last >= bookforge_core::ir::TEXT_NODE_PATH_BASE,
            "stray text block must use a text-node path component, got {last}"
        );
    }

    #[test]
    fn nested_same_name_blocks_stay_in_one_block() {
        let section_id = SectionId("sec_000000".to_string());
        let blocks = extract_blocks(
            "<html><body><ul><li>Outer <ul><li>Inner</li></ul> tail</li><li>Sibling</li></ul></body></html>",
            "chapter.xhtml",
            &section_id,
            0,
        )
        .expect("block extraction should succeed");

        assert_eq!(blocks.len(), 2, "outer li (with nested list) + sibling li");
        let outer = block_text(&blocks[0]);
        assert!(outer.contains("Outer"), "got: {outer}");
        assert!(
            outer.contains("Inner"),
            "nested li text stays inside the outer block: {outer}"
        );
        assert!(
            outer.contains("tail"),
            "text after the nested list must not be lost: {outer}"
        );
        assert_eq!(block_text(&blocks[1]), "Sibling");
    }

    #[test]
    fn named_html_entities_decode_in_text() {
        let section_id = SectionId("sec_000000".to_string());
        let blocks = extract_blocks(
            "<html><body><p>one&nbsp;two&mdash;three</p></body></html>",
            "chapter.xhtml",
            &section_id,
            0,
        )
        .expect("named entities must not fail extraction");

        assert_eq!(blocks.len(), 1);
        assert_eq!(block_text(&blocks[0]), "one two\u{2014}three");
    }

    #[test]
    fn script_and_style_text_is_never_extracted() {
        let section_id = SectionId("sec_000000".to_string());
        let blocks = extract_blocks(
            "<html><head><title>Meta</title><style>p { color: red; }</style></head><body><script>var x = 1;</script><div>Real</div></body></html>",
            "chapter.xhtml",
            &section_id,
            0,
        )
        .expect("block extraction should succeed");

        let texts = blocks.iter().map(block_text).collect::<Vec<_>>();
        assert_eq!(texts, vec!["Meta", "Real"]);
        assert!(!texts.iter().any(|text| text.contains("color")));
        assert!(!texts.iter().any(|text| text.contains("var x")));
    }

    #[test]
    fn navigation_list_labels_anchor_inside_links_not_on_list_items() {
        let section_id = SectionId("sec_000000".to_string());
        let blocks = extract_blocks(
            r#"<html xmlns="http://www.w3.org/1999/xhtml"><body>
<nav><ol><li><a href="chapter.xhtml">Cover</a></li>
<li><a href="part.xhtml"><span>Part One</span></a><ol><li><a href="one.xhtml"><span>1</span> Chapter One</a></li></ol></li></ol></nav>
</body></html>"#,
            "toc.xhtml",
            &section_id,
            0,
        )
        .expect("navigation should parse");

        let texts = blocks.iter().map(block_text).collect::<Vec<_>>();
        assert_eq!(texts, vec!["Cover", "Part One", "1", "Chapter One"]);
        assert!(
            blocks.iter().all(|block| block.dom_path.0.len() >= 6),
            "navigation labels should not be anchored on their li elements"
        );
    }

    #[test]
    fn visible_body_chars_counts_body_and_title_but_not_style() {
        let xhtml = r#"<html><head><title>Heading</title><style>p { color: red; }</style></head>
<body><p>captured</p><div>div text</div></body></html>"#;
        let total = visible_body_chars(xhtml).expect("count should succeed");
        // "Heading" (7) + "captured" (8) + "divtext" (7); style is excluded.
        // The head title counts because the extractor translates it too.
        assert_eq!(total, 22);
    }

    #[test]
    fn coverage_captures_div_text_and_reports_svg_labels_uncaptured() {
        let section_id = SectionId("sec_000000".to_string());
        let xhtml =
            "<html><body><p>in a block</p><div>also in a div</div><svg><text>diagram label</text></svg></body></html>"
                .to_string();
        let blocks =
            extract_blocks(&xhtml, "chapter.xhtml", &section_id, 0).expect("blocks should parse");
        let captured = blocks
            .iter()
            .map(|block| non_whitespace_chars(&block_visible_text(block)))
            .sum::<usize>();
        let total = visible_body_chars(&xhtml).expect("count should succeed");

        assert_eq!(
            captured,
            non_whitespace_chars("in a block") + non_whitespace_chars("also in a div"),
            "prose in p and div must both be captured"
        );
        assert_eq!(
            total - captured,
            non_whitespace_chars("diagram label"),
            "svg text stays uncaptured and visible in the coverage gap"
        );
    }

    #[test]
    fn protected_spans_do_not_overflag_single_digits() {
        let spans = detect_protected_spans(
            "Chapter 1 cites https://example.com, file.txt, #anchor, and pages 12-14.",
        );
        let texts = spans
            .iter()
            .map(|span| span.text.as_str())
            .collect::<Vec<_>>();

        assert!(!texts.contains(&"1"));
        assert!(texts.contains(&"https://example.com"));
        assert!(texts.contains(&"file.txt"));
        assert!(texts.contains(&"#anchor"));
        assert!(texts.contains(&"12-14"));
    }

    #[test]
    fn protected_spans_do_not_treat_sentence_fragments_as_filenames() {
        let spans = detect_protected_spans(
            "case.Fedor bow.At said:“The file.txt chapter.xhtml [@tolstoy1886] @note1",
        );
        let texts = spans
            .iter()
            .map(|span| span.text.as_str())
            .collect::<Vec<_>>();

        assert!(!texts.contains(&"case.Fedor"));
        assert!(!texts.contains(&"bow.At"));
        assert!(!texts.contains(&"said:“The"));
        assert!(texts.contains(&"file.txt"));
        assert!(texts.contains(&"chapter.xhtml"));
        assert!(texts.contains(&"[@tolstoy1886]"));
        assert!(texts.contains(&"@note1"));
    }

    fn block_text(block: &Block) -> String {
        block
            .text_runs
            .iter()
            .map(|run| run.text.as_str())
            .collect::<Vec<_>>()
            .join("")
    }
}
