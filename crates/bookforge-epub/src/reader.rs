use std::{
    collections::HashMap,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

use bookforge_core::{
    BookforgeError, Result,
    ir::{Book, BookFormat, BookId, Metadata, Resource, SpineItem},
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
    let package = parse_package(&package_xml)?;

    Ok(Book {
        id: BookId(package_path),
        format: BookFormat::Epub,
        metadata: package.metadata,
        manifest: package.manifest,
        spine: package.spine,
        sections: Vec::new(),
        blocks: Vec::new(),
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
