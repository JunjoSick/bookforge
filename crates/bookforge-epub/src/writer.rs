use std::{
    collections::HashMap,
    fs::File,
    io::{Read, Write},
    path::Path,
};

use bookforge_core::{
    BookforgeError, Result,
    ir::{Block, Book, DomPath},
    segment::BlockTranslation,
};
use quick_xml::{
    Reader, Writer,
    events::{BytesEnd, BytesText, Event},
};
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

pub fn rebuild_epub(book: &Book, translations: &[BlockTranslation], output: &Path) -> Result<()> {
    let source_path = book.source_path.as_deref().ok_or_else(|| {
        BookforgeError::InvalidInput("book IR does not include a source EPUB path".to_string())
    })?;
    let source = File::open(source_path)?;
    let mut archive = ZipArchive::new(source)?;
    let output_file = File::create(output)?;
    let mut writer = ZipWriter::new(output_file);

    let translations_by_block = translations
        .iter()
        .map(|translation| (&translation.block_id, translation.text.as_str()))
        .collect::<HashMap<_, _>>();
    let patches = book
        .blocks
        .iter()
        .filter_map(|block| {
            translations_by_block
                .get(&block.id)
                .map(|translation| (block, *translation))
        })
        .collect::<Vec<_>>();
    let patches_by_href = patches_by_href(book, &patches);

    write_mimetype_first(&mut archive, &mut writer)?;

    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for index in 0..archive.len() {
        let mut file = archive.by_index(index)?;
        let name = file.name().to_string();

        if name == "mimetype" {
            continue;
        }

        if file.is_dir() {
            writer.add_directory(name, deflated)?;
            continue;
        }

        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let output_bytes = if let Some(file_patches) = patches_by_href.get(name.as_str()) {
            let xhtml = String::from_utf8(bytes).map_err(|err| {
                BookforgeError::InvalidInput(format!("XHTML resource '{name}' is not UTF-8: {err}"))
            })?;
            patch_xhtml(&xhtml, file_patches)?.into_bytes()
        } else {
            bytes
        };

        writer.start_file(name, deflated)?;
        writer.write_all(&output_bytes)?;
    }

    writer.finish()?;
    Ok(())
}

fn write_mimetype_first(source: &mut ZipArchive<File>, writer: &mut ZipWriter<File>) -> Result<()> {
    let mut mimetype = String::new();
    source.by_name("mimetype")?.read_to_string(&mut mimetype)?;
    if mimetype.trim() != "application/epub+zip" {
        return Err(BookforgeError::InvalidInput(
            "EPUB mimetype must be application/epub+zip".to_string(),
        ));
    }

    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    writer.start_file("mimetype", stored)?;
    writer.write_all(b"application/epub+zip")?;
    Ok(())
}

fn patches_by_href<'a>(
    book: &'a Book,
    patches: &'a [(&'a Block, &'a str)],
) -> HashMap<&'a str, Vec<(&'a DomPath, &'a str)>> {
    let section_href = book
        .sections
        .iter()
        .map(|section| (&section.id, section.href.as_str()))
        .collect::<HashMap<_, _>>();
    let mut by_href = HashMap::<&str, Vec<(&DomPath, &str)>>::new();

    for (block, translation) in patches {
        if let Some(href) = section_href.get(&block.section_id) {
            by_href
                .entry(*href)
                .or_default()
                .push((&block.dom_path, *translation));
        }
    }

    by_href
}

#[derive(Debug)]
struct ElementFrame {
    path: Vec<usize>,
    child_count: usize,
}

fn patch_xhtml(xhtml: &str, patches: &[(&DomPath, &str)]) -> Result<String> {
    let patch_map = patches
        .iter()
        .map(|(path, text)| (path.0.as_slice(), *text))
        .collect::<HashMap<_, _>>();
    let mut reader = Reader::from_str(xhtml);
    reader.config_mut().trim_text(false);
    let mut writer = Writer::new(Vec::new());
    let mut stack = Vec::<ElementFrame>::new();
    let mut replacement_depth: Option<usize> = None;

    loop {
        match reader.read_event()? {
            Event::Start(element) => {
                if let Some(depth) = replacement_depth.as_mut() {
                    *depth += 1;
                    continue;
                }

                let path = enter_element(&mut stack);
                let replacement = patch_map.get(path.as_slice()).copied();
                writer.write_event(Event::Start(element.borrow()))?;
                if let Some(text) = replacement {
                    writer.write_event(Event::Text(BytesText::new(text)))?;
                    replacement_depth = Some(0);
                }
            }
            Event::Empty(element) => {
                if replacement_depth.is_some() {
                    continue;
                }

                let path = next_child_path(&mut stack);
                if let Some(text) = patch_map.get(path.as_slice()).copied() {
                    let name = element.name();
                    writer.write_event(Event::Start(element.borrow()))?;
                    writer.write_event(Event::Text(BytesText::new(text)))?;
                    writer.write_event(Event::End(BytesEnd::new(
                        String::from_utf8_lossy(name.as_ref()).as_ref(),
                    )))?;
                } else {
                    writer.write_event(Event::Empty(element.borrow()))?;
                }
            }
            Event::End(element) => {
                if let Some(depth) = replacement_depth.as_mut() {
                    if *depth == 0 {
                        writer.write_event(Event::End(element.borrow()))?;
                        replacement_depth = None;
                        stack.pop();
                    } else {
                        *depth -= 1;
                    }
                    continue;
                }

                writer.write_event(Event::End(element.borrow()))?;
                stack.pop();
            }
            Event::Eof => break,
            event => {
                if replacement_depth.is_none() {
                    writer.write_event(event.borrow())?;
                }
            }
        }
    }

    String::from_utf8(writer.into_inner()).map_err(|err| {
        BookforgeError::InvalidInput(format!("patched XHTML is not valid UTF-8: {err}"))
    })
}

fn enter_element(stack: &mut Vec<ElementFrame>) -> Vec<usize> {
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
