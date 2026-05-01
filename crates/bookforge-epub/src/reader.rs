use std::{
    collections::HashMap,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

use bookforge_core::{
    BookforgeError, Result,
    ir::{
        Block, BlockId, BlockKind, Book, BookFormat, BookId, DomPath, InlineMark, Metadata,
        ProtectedSpan, ProtectedSpanKind, Resource, Section, SectionId, SpineItem, TextRun,
    },
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
                        .decode()
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
    path: Vec<usize>,
    child_count: usize,
}

#[derive(Debug)]
struct BlockBuilder {
    element_name: Vec<u8>,
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
    fn new(element_name: Vec<u8>, kind: BlockKind, dom_path: DomPath, ordinal: usize) -> Self {
        Self {
            element_name,
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
        let id = marker_id(b"m", self.ordinal, self.next_marker);
        self.next_marker += 1;
        self.inline_marks.push(InlineMark {
            id: id.clone(),
            kind: String::from_utf8_lossy(name).into_owned(),
        });
        self.inline_stack.push(id.clone());
        self.push_run(format!("<m id=\"{id}\">"));
    }

    fn push_inline_empty(&mut self, name: &[u8]) {
        let id = marker_id(b"r", self.ordinal, self.next_marker);
        self.next_marker += 1;
        self.inline_marks.push(InlineMark {
            id: id.clone(),
            kind: String::from_utf8_lossy(name).into_owned(),
        });
        self.push_run(format!("<ref id=\"{id}\"/>"));
    }

    fn push_inline_end(&mut self) {
        if self.inline_stack.pop().is_some() {
            self.push_run("</m>".to_string());
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

    loop {
        match reader.read_event()? {
            Event::Start(element) => {
                let name = local_name(element.name().as_ref()).to_vec();
                let path = enter_element(&mut element_stack, &name);

                if active_block.is_none()
                    && let Some(kind) = block_kind(&name, &element)?
                {
                    active_block = Some(BlockBuilder::new(
                        name,
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
                let path = next_child_path(&mut element_stack);

                if let Some(block) = active_block.as_mut() {
                    block.push_inline_empty(&name);
                } else if let Some(kind) = block_kind(&name, &element)? {
                    let block = build_block(
                        section_id,
                        initial_block_count + blocks.len(),
                        kind,
                        DomPath(path),
                        Vec::new(),
                        Vec::new(),
                        String::new(),
                    );
                    blocks.push(block);
                }
            }
            Event::Text(text) => {
                if let Some(block) = active_block.as_mut() {
                    let value = text
                        .decode()
                        .map_err(|err| BookforgeError::InvalidInput(err.to_string()))?;
                    block.push_text(&value);
                }
            }
            Event::CData(text) => {
                if let Some(block) = active_block.as_mut() {
                    let value = text
                        .decode()
                        .map_err(|err| BookforgeError::InvalidInput(err.to_string()))?;
                    block.push_text(&value);
                }
            }
            Event::End(element) => {
                let name = local_name(element.name().as_ref()).to_vec();
                let should_finish = active_block
                    .as_ref()
                    .is_some_and(|block| block.element_name == name);

                if should_finish {
                    let block = active_block.take().expect("checked above");
                    if let Some(block) = block.finish(section_id) {
                        blocks.push(block);
                    }
                } else if let Some(block) = active_block.as_mut() {
                    block.push_inline_end();
                }

                element_stack.pop();
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(blocks)
}

fn enter_element(stack: &mut Vec<ElementFrame>, _name: &[u8]) -> Vec<usize> {
    let path = next_child_path(stack);
    stack.push(ElementFrame {
        path: path.clone(),
        child_count: 0,
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
    strip_marker_tokens(&marked)
}

fn strip_marker_tokens(text: &str) -> String {
    let mut output = String::new();
    let mut rest = text;

    while let Some(index) = rest.find('<') {
        output.push_str(&rest[..index]);
        let tag = &rest[index..];

        if (tag.starts_with("<m id=\"") || tag.starts_with("<ref id=\"") || tag.starts_with("</m>"))
            && let Some(end) = tag.find('>')
        {
            rest = &tag[end + 1..];
            continue;
        }

        output.push('<');
        rest = &tag[1..];
    }

    output.push_str(rest);
    normalize_space(&output)
}

fn is_marker_token(text: &str) -> bool {
    matches!(text, "</m>") || text.starts_with("<m id=\"") || text.starts_with("<ref id=\"")
}

fn marker_id(prefix: &[u8], block_ordinal: usize, marker_ordinal: usize) -> String {
    format!(
        "{}{block_ordinal:06}_{marker_ordinal:03}",
        String::from_utf8_lossy(prefix)
    )
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
    if base.is_empty() {
        normalize_epub_path(href)
    } else {
        normalize_epub_path(&format!("{base}/{href}"))
    }
}

fn normalize_epub_path(path: &str) -> String {
    let mut normalized = PathBuf::new();
    for component in Path::new(path).components() {
        normalized.push(component.as_os_str());
    }
    normalized.to_string_lossy().replace('\\', "/")
}

fn local_name(name: &[u8]) -> &[u8] {
    name.rsplit(|byte| *byte == b':').next().unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(text, "Hello <m id=\"m000000_000\">world</m>!");
        assert_eq!(blocks[0].inline_marks.len(), 1);
        assert_eq!(blocks[0].inline_marks[0].id, "m000000_000");
        assert_eq!(blocks[0].inline_marks[0].kind, "em");
        assert_eq!(blocks[0].token_estimate, estimate_tokens("Hello world!"));
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
        assert_eq!(block_text(&blocks[0]), "Line<ref id=\"r000004_000\"/>break");
        assert_eq!(blocks[0].inline_marks[0].id, "r000004_000");
        assert_eq!(blocks[0].inline_marks[0].kind, "br");
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
