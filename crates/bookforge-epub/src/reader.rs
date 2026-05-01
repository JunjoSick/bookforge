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

    Ok(Book {
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
            Event::End(element) => {
                if current_text_element
                    .as_deref()
                    .is_some_and(|name| name == local_name(element.name().as_ref()))
                {
                    current_text_element = None;
                }
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
    text: String,
}

fn extract_blocks(
    xhtml: &str,
    href: &str,
    section_id: &SectionId,
    initial_block_count: usize,
) -> Result<Vec<Block>> {
    let mut reader = Reader::from_str(xhtml);
    reader.config_mut().trim_text(true);

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
                    active_block = Some(BlockBuilder {
                        element_name: name,
                        kind,
                        dom_path: DomPath(path),
                        text: String::new(),
                    });
                }
            }
            Event::Empty(element) => {
                let name = local_name(element.name().as_ref()).to_vec();
                let path = next_child_path(&mut element_stack);

                if active_block.is_none()
                    && let Some(kind) = block_kind(&name, &element)?
                {
                    let block = build_block(
                        section_id,
                        initial_block_count + blocks.len(),
                        kind,
                        DomPath(path),
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
                    push_text(&mut block.text, value.trim());
                }
            }
            Event::CData(text) => {
                if let Some(block) = active_block.as_mut() {
                    let value = text
                        .decode()
                        .map_err(|err| BookforgeError::InvalidInput(err.to_string()))?;
                    push_text(&mut block.text, value.trim());
                }
            }
            Event::End(element) => {
                let name = local_name(element.name().as_ref()).to_vec();
                let should_finish = active_block
                    .as_ref()
                    .is_some_and(|block| block.element_name == name);

                if should_finish {
                    let block = active_block.take().expect("checked above");
                    let text = normalize_space(&block.text);
                    if !text.is_empty() {
                        blocks.push(build_block(
                            section_id,
                            initial_block_count + blocks.len(),
                            block.kind,
                            block.dom_path,
                            text,
                        ));
                    }
                }

                element_stack.pop();
            }
            Event::Eof => break,
            _ => {}
        }
    }

    if blocks.is_empty() {
        return Err(BookforgeError::InvalidInput(format!(
            "XHTML spine resource '{href}' contains no translatable blocks"
        )));
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
    text: String,
) -> Block {
    let text_runs = vec![TextRun {
        id: "r0".to_string(),
        text: text.clone(),
    }];
    let protected_spans = detect_protected_spans(&text);

    Block {
        id: BlockId(format!("b_{ordinal:06}")),
        section_id: section_id.clone(),
        kind,
        dom_path,
        text_runs,
        inline_marks: Vec::<InlineMark>::new(),
        protected_spans,
        token_estimate: estimate_tokens(&text),
    }
}

fn first_heading(blocks: &[Block]) -> (Option<String>, Option<u8>) {
    blocks
        .iter()
        .find_map(|block| match block.kind {
            BlockKind::Heading(level) => Some((
                block.text_runs.first().map(|run| run.text.clone()),
                Some(level),
            )),
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

fn push_text(output: &mut String, text: &str) {
    if text.is_empty() {
        return;
    }

    if !output.is_empty() {
        output.push(' ');
    }
    output.push_str(text);
}

fn normalize_space(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn estimate_tokens(text: &str) -> usize {
    let words = text.split_whitespace().count();
    words.saturating_mul(4).div_ceil(3).max(1)
}

fn detect_protected_spans(text: &str) -> Vec<ProtectedSpan> {
    text.split_whitespace()
        .filter_map(|raw| {
            let value = raw.trim_matches(|ch: char| {
                matches!(
                    ch,
                    ',' | ';' | ':' | '.' | '!' | '?' | '(' | ')' | '[' | ']'
                )
            });
            protected_span_kind(value).map(|kind| ProtectedSpan {
                kind,
                text: value.to_string(),
            })
        })
        .collect()
}

fn protected_span_kind(value: &str) -> Option<ProtectedSpanKind> {
    if value.starts_with("http://") || value.starts_with("https://") {
        Some(ProtectedSpanKind::Url)
    } else if value.contains('@') && value.contains('.') {
        Some(ProtectedSpanKind::Email)
    } else if value.chars().all(|ch| ch.is_ascii_digit()) {
        Some(ProtectedSpanKind::Number)
    } else if value.contains('.') && !value.starts_with('.') && !value.ends_with('.') {
        Some(ProtectedSpanKind::Filename)
    } else {
        None
    }
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
