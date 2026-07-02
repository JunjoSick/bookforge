use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use bookforge_core::{
    BookforgeError, Result,
    ir::{Block, Book, DomPath, TEXT_NODE_PATH_BASE},
    marker::{parse_empty_marker, parse_paired_marker_open},
    segment::BlockTranslation,
};
use quick_xml::{
    Reader, Writer,
    events::{BytesEnd, BytesText, Event},
};
use zip::{CompressionMethod, DateTime, ZipArchive, ZipWriter, write::SimpleFileOptions};

pub fn rebuild_epub(book: &Book, translations: &[BlockTranslation], output: &Path) -> Result<()> {
    rebuild_epub_with_language(book, translations, output, None)
}

pub fn rebuild_epub_with_language(
    book: &Book,
    translations: &[BlockTranslation],
    output: &Path,
    target_language: Option<&str>,
) -> Result<()> {
    let staged = sibling_work_path(output, "tmp");
    let result = write_rebuilt_epub(book, translations, &staged, target_language);
    let skipped = match result {
        Ok(skipped) => skipped,
        Err(error) => {
            let _ = fs::remove_file(&staged);
            return Err(error);
        }
    };
    if let Err(error) = commit_staged_output(&staged, output) {
        let _ = fs::remove_file(&staged);
        return Err(error);
    }

    if skipped > 0 {
        tracing::warn!(
            skipped_blocks = skipped,
            "rebuild left {skipped} block(s) untranslated to preserve inline structure"
        );
    }

    Ok(())
}

fn write_rebuilt_epub(
    book: &Book,
    translations: &[BlockTranslation],
    output: &Path,
    target_language: Option<&str>,
) -> Result<usize> {
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

    let deflated = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .last_modified_time(deterministic_zip_time());
    let mut total_skipped = 0usize;
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
            let outcome = patch_xhtml_blocks(&xhtml, file_patches)?;
            total_skipped += outcome.skipped_blocks;
            let xhtml = if name == book.id.0 {
                if let Some(target_language) = target_language {
                    patch_opf_language(&outcome.xhtml, target_language)?
                } else {
                    outcome.xhtml
                }
            } else {
                outcome.xhtml
            };
            validate_xml(&xhtml).map_err(|err| {
                BookforgeError::InvalidInput(format!(
                    "patched XHTML '{name}' failed validation: {err}"
                ))
            })?;
            xhtml.into_bytes()
        } else if name == book.id.0 {
            if let Some(target_language) = target_language {
                let opf = String::from_utf8(bytes).map_err(|err| {
                    BookforgeError::InvalidInput(format!(
                        "OPF resource '{name}' is not UTF-8: {err}"
                    ))
                })?;
                let opf = patch_opf_language(&opf, target_language)?;
                validate_xml(&opf).map_err(|err| {
                    BookforgeError::InvalidInput(format!(
                        "patched OPF '{name}' failed validation: {err}"
                    ))
                })?;
                opf.into_bytes()
            } else {
                bytes
            }
        } else {
            bytes
        };

        writer.start_file(name, deflated)?;
        writer.write_all(&output_bytes)?;
    }

    writer.finish()?;

    Ok(total_skipped)
}

fn commit_staged_output(staged: &Path, output: &Path) -> Result<()> {
    if !output.exists() {
        fs::rename(staged, output)?;
        return Ok(());
    }

    let backup = sibling_work_path(output, "backup");
    fs::rename(output, &backup)?;
    match fs::rename(staged, output) {
        Ok(()) => {
            if let Err(error) = fs::remove_file(&backup) {
                tracing::warn!(
                    backup = %backup.display(),
                    error = %error,
                    "rebuilt EPUB is committed but its backup could not be removed"
                );
            }
            Ok(())
        }
        Err(error) => {
            let _ = fs::rename(&backup, output);
            Err(error.into())
        }
    }
}

fn sibling_work_path(output: &Path, label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let name = output
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("book.epub");
    output.with_file_name(format!(
        ".{name}.bookforge-{label}-{}-{nonce}",
        std::process::id()
    ))
}

fn write_mimetype_first(source: &mut ZipArchive<File>, writer: &mut ZipWriter<File>) -> Result<()> {
    let mut mimetype = String::new();
    source.by_name("mimetype")?.read_to_string(&mut mimetype)?;
    if mimetype.trim() != "application/epub+zip" {
        return Err(BookforgeError::InvalidInput(
            "EPUB mimetype must be application/epub+zip".to_string(),
        ));
    }

    let stored = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Stored)
        .last_modified_time(deterministic_zip_time());
    writer.start_file("mimetype", stored)?;
    writer.write_all(b"application/epub+zip")?;
    Ok(())
}

fn deterministic_zip_time() -> DateTime {
    DateTime::from_date_and_time(1980, 1, 1, 0, 0, 0).expect("DOS epoch timestamp should be valid")
}

fn patches_by_href<'a>(
    book: &'a Book,
    patches: &'a [(&'a Block, &'a str)],
) -> HashMap<&'a str, Vec<BlockPatch<'a>>> {
    let section_href = book
        .sections
        .iter()
        .map(|section| (&section.id, section.href.as_str()))
        .collect::<HashMap<_, _>>();
    let mut by_href = HashMap::<&str, Vec<BlockPatch<'a>>>::new();

    for (block, translation) in patches {
        if let Some(href) = section_href.get(&block.section_id) {
            by_href
                .entry(*href)
                .or_default()
                .push(BlockPatch { block, translation });
        }
    }

    by_href
}

fn patch_opf_language(opf: &str, target_language: &str) -> Result<String> {
    let language_tag = epub_language_tag(target_language);
    let mut reader = Reader::from_str(opf);
    reader.config_mut().trim_text(false);
    let mut writer = Writer::new(Vec::new());
    let mut in_language = false;
    let mut wrote_language = false;
    let mut found_language = false;

    loop {
        match reader.read_event()? {
            Event::Start(element) if local_name(element.name().as_ref()) == b"language" => {
                found_language = true;
                in_language = true;
                wrote_language = false;
                writer.write_event(Event::Start(element))?;
            }
            Event::Empty(element) if local_name(element.name().as_ref()) == b"language" => {
                found_language = true;
                writer.write_event(Event::Start(element.to_owned()))?;
                writer.write_event(Event::Text(BytesText::new(&language_tag)))?;
                writer.write_event(Event::End(element.to_end()))?;
            }
            Event::Text(_) | Event::CData(_) | Event::GeneralRef(_) if in_language => {
                if !wrote_language {
                    writer.write_event(Event::Text(BytesText::new(&language_tag)))?;
                    wrote_language = true;
                }
            }
            Event::End(element)
                if in_language && local_name(element.name().as_ref()) == b"language" =>
            {
                if !wrote_language {
                    writer.write_event(Event::Text(BytesText::new(&language_tag)))?;
                }
                in_language = false;
                writer.write_event(Event::End(element))?;
            }
            Event::Eof => break,
            event => writer.write_event(event)?,
        }
    }

    if !found_language {
        return Ok(opf.to_string());
    }

    String::from_utf8(writer.into_inner()).map_err(|err| {
        BookforgeError::InvalidInput(format!("patched OPF language is not valid UTF-8: {err}"))
    })
}

fn epub_language_tag(language: &str) -> String {
    let trimmed = language.trim();
    if trimmed.is_empty() {
        return trimmed.to_string();
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
        && (lower.len() <= 3 || lower.contains('-'))
    {
        return lower;
    }
    match lower.as_str() {
        "english" => "en",
        "italian" => "it",
        "french" => "fr",
        "german" => "de",
        "spanish" => "es",
        "portuguese" => "pt",
        "brazilian portuguese" | "portuguese (brazil)" => "pt-BR",
        "japanese" => "ja",
        "chinese" => "zh",
        "korean" => "ko",
        "russian" => "ru",
        "arabic" => "ar",
        "dutch" => "nl",
        "polish" => "pl",
        "swedish" => "sv",
        "norwegian" => "no",
        "danish" => "da",
        "finnish" => "fi",
        _ => trimmed,
    }
    .to_string()
}

fn local_name(name: &[u8]) -> &[u8] {
    name.rsplit(|byte| *byte == b':').next().unwrap_or(name)
}

#[derive(Debug, Clone, Copy)]
struct BlockPatch<'a> {
    block: &'a Block,
    translation: &'a str,
}

#[derive(Debug, Clone, Copy)]
struct PatchSpec<'a> {
    dom_path: &'a DomPath,
    block: Option<&'a Block>,
    translation: &'a str,
}

#[derive(Debug)]
struct ElementFrame {
    path: Vec<usize>,
    child_count: usize,
    text_count: usize,
}

#[derive(Debug)]
pub(crate) struct PatchOutcome {
    pub xhtml: String,
    pub skipped_blocks: usize,
}

#[cfg(test)]
pub(crate) fn patch_xhtml(xhtml: &str, patches: &[(&DomPath, &str)]) -> Result<PatchOutcome> {
    let specs = patches
        .iter()
        .map(|(dom_path, translation)| PatchSpec {
            dom_path,
            block: None,
            translation,
        })
        .collect::<Vec<_>>();
    patch_xhtml_with_specs(xhtml, &specs)
}

fn patch_xhtml_blocks(xhtml: &str, patches: &[BlockPatch<'_>]) -> Result<PatchOutcome> {
    let specs = patches
        .iter()
        .map(|patch| PatchSpec {
            dom_path: &patch.block.dom_path,
            block: Some(patch.block),
            translation: patch.translation,
        })
        .collect::<Vec<_>>();
    patch_xhtml_with_specs(xhtml, &specs)
}

fn patch_xhtml_with_specs(xhtml: &str, patches: &[PatchSpec<'_>]) -> Result<PatchOutcome> {
    let patch_map = patches
        .iter()
        .map(|patch| (patch.dom_path.0.as_slice(), *patch))
        .collect::<HashMap<_, _>>();
    let mut reader = Reader::from_str(xhtml);
    reader.config_mut().trim_text(false);
    let mut writer = Writer::new(Vec::new());
    let mut stack = Vec::<ElementFrame>::new();
    let mut skipped_blocks = 0usize;

    loop {
        match reader.read_event()? {
            Event::Start(element) => {
                let path = enter_element(&mut stack);
                writer.write_event(Event::Start(element.borrow()))?;

                if let Some(patch) = patch_map.get(path.as_slice()).copied() {
                    let buffered = buffer_until_matching_end(&mut reader)?;
                    if buffered.has_inline_children {
                        match patch.block.map(|block| {
                            render_marked_translation(block, patch.translation, &buffered.events)
                        }) {
                            Some(Ok(events)) => {
                                for event in &events {
                                    writer.write_event(event.borrow())?;
                                }
                            }
                            Some(Err(error)) => {
                                skipped_blocks += 1;
                                tracing::warn!(
                                    block_path = ?path,
                                    error = %error,
                                    "preserving original block contents: translated inline markers \
                                     could not be applied",
                                );
                                for event in &buffered.events {
                                    writer.write_event(event.borrow())?;
                                }
                            }
                            None => {
                                skipped_blocks += 1;
                                tracing::warn!(
                                    block_path = ?path,
                                    "preserving original block contents: inline patch did not include \
                                     block marker metadata",
                                );
                                for event in &buffered.events {
                                    writer.write_event(event.borrow())?;
                                }
                            }
                        }
                    } else {
                        writer.write_event(Event::Text(BytesText::new(patch.translation)))?;
                    }
                    writer.write_event(Event::End(buffered.end.borrow()))?;
                    stack.pop();
                }
            }
            Event::Empty(element) => {
                let path = next_child_path(&mut stack);
                if let Some(patch) = patch_map.get(path.as_slice()).copied() {
                    let name = element.name();
                    let name_str = String::from_utf8_lossy(name.as_ref()).into_owned();
                    writer.write_event(Event::Start(element.borrow()))?;
                    writer.write_event(Event::Text(BytesText::new(patch.translation)))?;
                    writer.write_event(Event::End(BytesEnd::new(name_str)))?;
                } else {
                    writer.write_event(Event::Empty(element.borrow()))?;
                }
            }
            Event::End(element) => {
                writer.write_event(Event::End(element.borrow()))?;
                stack.pop();
            }
            // Text nodes are addressable patch targets: the reader emits
            // standalone blocks for non-whitespace text the block
            // whitelist missed, addressed as parent path plus
            // TEXT_NODE_PATH_BASE + n. Counting must mirror the reader:
            // every text node that is non-whitespace after entity
            // decoding consumes one index in its parent frame.
            Event::Text(text) => {
                let non_whitespace = text
                    .html_content()
                    .map(|value| !value.trim().is_empty())
                    .unwrap_or(true);
                match text_node_patch(&patch_map, &mut stack, non_whitespace) {
                    Some(translation) => {
                        writer.write_event(Event::Text(BytesText::new(translation)))?
                    }
                    None => writer.write_event(Event::Text(text.borrow()))?,
                }
            }
            Event::CData(text) => {
                let non_whitespace = text
                    .decode()
                    .map(|value| !value.trim().is_empty())
                    .unwrap_or(true);
                match text_node_patch(&patch_map, &mut stack, non_whitespace) {
                    Some(translation) => {
                        writer.write_event(Event::Text(BytesText::new(translation)))?
                    }
                    None => writer.write_event(Event::CData(text.borrow()))?,
                }
            }
            Event::Eof => break,
            event => {
                writer.write_event(event.borrow())?;
            }
        }
    }

    let xhtml = String::from_utf8(writer.into_inner()).map_err(|err| {
        BookforgeError::InvalidInput(format!("patched XHTML is not valid UTF-8: {err}"))
    })?;

    Ok(PatchOutcome {
        xhtml,
        skipped_blocks,
    })
}

struct BufferedBlock {
    events: Vec<Event<'static>>,
    end: BytesEnd<'static>,
    has_inline_children: bool,
}

fn buffer_until_matching_end(reader: &mut Reader<&[u8]>) -> Result<BufferedBlock> {
    let mut events = Vec::new();
    let mut depth = 0usize;
    let mut has_inline_children = false;

    loop {
        match reader.read_event()? {
            Event::Start(element) => {
                depth += 1;
                has_inline_children = true;
                events.push(Event::Start(element).into_owned());
            }
            Event::Empty(element) => {
                has_inline_children = true;
                events.push(Event::Empty(element).into_owned());
            }
            Event::End(element) => {
                if depth == 0 {
                    return Ok(BufferedBlock {
                        events,
                        end: element.into_owned(),
                        has_inline_children,
                    });
                }
                depth -= 1;
                events.push(Event::End(element).into_owned());
            }
            Event::Eof => {
                return Err(BookforgeError::InvalidInput(
                    "unexpected end of XHTML while buffering block contents".to_string(),
                ));
            }
            event => events.push(event.into_owned()),
        }
    }
}

#[derive(Debug, Clone)]
enum InlineTemplate {
    Paired {
        start: Event<'static>,
        end: BytesEnd<'static>,
    },
    Empty(Event<'static>),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct InlineWhitespaceBoundary {
    left: String,
    right: String,
}

#[derive(Debug, Clone)]
struct RenderedEvent {
    event: Event<'static>,
    edge: Option<MarkerEdge>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MarkerEdge {
    Start(String),
    End(String),
}

impl RenderedEvent {
    fn plain(event: Event<'static>) -> Self {
        Self { event, edge: None }
    }

    fn marker(event: Event<'static>, edge: MarkerEdge) -> Self {
        Self {
            event,
            edge: Some(edge),
        }
    }
}

fn normalize_marker_whitespace(text: &str) -> String {
    text.replace("</ m>", "</m>")
        .replace("</m >", "</m>")
        .replace("</ m >", "</m>")
        .replace("</ keep>", "</keep>")
        .replace("</keep >", "</keep>")
        .replace("</ keep >", "</keep>")
}

fn render_marked_translation(
    block: &Block,
    translation: &str,
    original_events: &[Event<'static>],
) -> Result<Vec<Event<'static>>> {
    let normalized = normalize_marker_whitespace(translation);
    let translation = normalized.as_str();
    let templates = collect_inline_templates(block, original_events)?;
    if templates.is_empty() {
        return Ok(vec![Event::Text(BytesText::new(translation).into_owned())]);
    }
    let inline_whitespace_boundaries = collect_inline_whitespace_boundaries(original_events)?;

    let mut rendered = Vec::new();
    let mut used = Vec::new();
    push_marked_fragment(translation, &templates, &mut rendered, &mut used)?;

    let mut expected = templates.keys().cloned().collect::<Vec<_>>();
    expected.sort();
    used.sort();
    used.dedup();

    if expected != used {
        let missing = expected
            .iter()
            .filter(|id| !used.contains(id))
            .cloned()
            .collect::<Vec<_>>();
        return Err(BookforgeError::InvalidInput(format!(
            "translation is missing required inline marker(s): {}",
            missing.join(", ")
        )));
    }

    restore_inline_boundary_spaces(&mut rendered, &inline_whitespace_boundaries);
    Ok(rendered
        .into_iter()
        .map(|rendered| rendered.event)
        .collect())
}

fn collect_inline_templates(
    block: &Block,
    events: &[Event<'static>],
) -> Result<HashMap<String, InlineTemplate>> {
    let mut marker_ordinal = 0usize;
    let mut templates = HashMap::new();
    let mut stack = Vec::<(String, Event<'static>)>::new();

    for event in events {
        match event {
            Event::Start(element) => {
                let id = marker_id("m", marker_ordinal);
                marker_ordinal += 1;
                stack.push((id, Event::Start(element.clone())));
            }
            Event::Empty(element) => {
                let id = marker_id("r", marker_ordinal);
                marker_ordinal += 1;
                templates.insert(id, InlineTemplate::Empty(Event::Empty(element.clone())));
            }
            Event::End(end) => {
                let Some((id, start)) = stack.pop() else {
                    return Err(BookforgeError::InvalidInput(format!(
                        "inline template stack underflow in block '{}' at path {:?}. The EPUB may have unbalanced inline markup.",
                        block.id.0, block.dom_path
                    )));
                };
                templates.insert(
                    id,
                    InlineTemplate::Paired {
                        start,
                        end: end.clone(),
                    },
                );
            }
            _ => {}
        }
    }

    if !stack.is_empty() {
        return Err(BookforgeError::InvalidInput(
            "inline template stack was not empty after collecting original events".to_string(),
        ));
    }

    Ok(templates)
}

fn collect_inline_whitespace_boundaries(
    events: &[Event<'static>],
) -> Result<HashSet<InlineWhitespaceBoundary>> {
    let mut marker_ordinal = 0usize;
    let mut stack = Vec::<String>::new();
    let mut boundaries = HashSet::new();
    let mut pending_left = None::<String>;
    let mut saw_whitespace = false;

    for event in events {
        match event {
            Event::Start(_) => {
                let id = marker_id("m", marker_ordinal);
                marker_ordinal += 1;
                insert_pending_boundary(&mut boundaries, pending_left.take(), saw_whitespace, &id);
                saw_whitespace = false;
                stack.push(id);
            }
            Event::Empty(_) => {
                let id = marker_id("r", marker_ordinal);
                marker_ordinal += 1;
                insert_pending_boundary(&mut boundaries, pending_left.take(), saw_whitespace, &id);
                pending_left = Some(id);
                saw_whitespace = false;
            }
            Event::End(_) => {
                let Some(id) = stack.pop() else {
                    return Err(BookforgeError::InvalidInput(
                        "inline whitespace stack underflow while collecting original boundaries"
                            .to_string(),
                    ));
                };
                pending_left = Some(id);
                saw_whitespace = false;
            }
            _ => {
                if let Some(is_whitespace) = event_text_is_whitespace(event)?
                    && pending_left.is_some()
                {
                    if is_whitespace {
                        saw_whitespace = true;
                    } else {
                        pending_left = None;
                        saw_whitespace = false;
                    }
                }
            }
        }
    }

    if !stack.is_empty() {
        return Err(BookforgeError::InvalidInput(
            "inline whitespace stack was not empty after collecting original boundaries"
                .to_string(),
        ));
    }

    Ok(boundaries)
}

fn insert_pending_boundary(
    boundaries: &mut HashSet<InlineWhitespaceBoundary>,
    left: Option<String>,
    saw_whitespace: bool,
    right: &str,
) {
    if let Some(left) = left
        && saw_whitespace
    {
        boundaries.insert(InlineWhitespaceBoundary {
            left,
            right: right.to_string(),
        });
    }
}

fn event_text_is_whitespace(event: &Event<'static>) -> Result<Option<bool>> {
    let value = match event {
        Event::Text(text) => text
            .html_content()
            .map_err(|err| BookforgeError::InvalidInput(err.to_string()))?
            .into_owned(),
        Event::CData(text) => text
            .decode()
            .map_err(|err| BookforgeError::InvalidInput(err.to_string()))?
            .into_owned(),
        Event::GeneralRef(reference) => {
            if let Some(ch) = reference
                .resolve_char_ref()
                .map_err(|err| BookforgeError::InvalidInput(err.to_string()))?
            {
                ch.to_string()
            } else {
                let name = reference
                    .decode()
                    .map_err(|err| BookforgeError::InvalidInput(err.to_string()))?;
                quick_xml::escape::resolve_html5_entity(&name)
                    .unwrap_or(&name)
                    .to_string()
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(value.chars().all(char::is_whitespace)))
}

fn restore_inline_boundary_spaces(
    rendered: &mut Vec<RenderedEvent>,
    boundaries: &HashSet<InlineWhitespaceBoundary>,
) {
    if boundaries.is_empty() {
        return;
    }

    let mut index = 0usize;
    while index + 1 < rendered.len() {
        let boundary = match (&rendered[index].edge, &rendered[index + 1].edge) {
            (Some(MarkerEdge::End(left)), Some(MarkerEdge::Start(right))) => {
                InlineWhitespaceBoundary {
                    left: left.clone(),
                    right: right.clone(),
                }
            }
            _ => {
                index += 1;
                continue;
            }
        };

        if boundaries.contains(&boundary) && boundary_needs_space(rendered, index) {
            rendered.insert(
                index + 1,
                RenderedEvent::plain(Event::Text(BytesText::new(" ").into_owned())),
            );
            index += 1;
        }
        index += 1;
    }
}

fn boundary_needs_space(rendered: &[RenderedEvent], left_edge_index: usize) -> bool {
    let previous = last_visible_char_before(rendered, left_edge_index);
    let next = first_visible_char_after(rendered, left_edge_index + 1);

    previous.is_some_and(is_word_char) && next.is_some_and(is_word_char)
}

fn last_visible_char_before(rendered: &[RenderedEvent], index: usize) -> Option<char> {
    rendered
        .iter()
        .take(index + 1)
        .rev()
        .find_map(|rendered| event_text(&rendered.event).and_then(|text| text.chars().next_back()))
}

fn first_visible_char_after(rendered: &[RenderedEvent], index: usize) -> Option<char> {
    rendered
        .iter()
        .skip(index)
        .find_map(|rendered| event_text(&rendered.event).and_then(|text| text.chars().next()))
}

fn event_text(event: &Event<'static>) -> Option<String> {
    match event {
        Event::Text(text) => text.decode().ok().map(|text| text.into_owned()),
        Event::CData(text) => text.decode().ok().map(|text| text.into_owned()),
        _ => None,
    }
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric()
}

fn push_marked_fragment(
    mut text: &str,
    templates: &HashMap<String, InlineTemplate>,
    output: &mut Vec<RenderedEvent>,
    used: &mut Vec<String>,
) -> Result<()> {
    while let Some(index) = text.find('<') {
        push_text_event(&text[..index], output);
        let tag = &text[index..];

        if let Some(open) = parse_paired_marker_open(tag) {
            let tag_name = open.tag_name;
            let id = open.id;
            if used.iter().any(|seen| seen == &id) {
                return Err(BookforgeError::InvalidInput(format!(
                    "translation contains a duplicate formatting marker '{id}'. The LLM copied the marker twice."
                )));
            }

            let after_open = &tag[open.len..];
            let close_start = find_matching_marker_close(after_open, &tag_name)?;
            let close_len = format!("</{tag_name}>").len();
            let inner = &after_open[..close_start];
            let after_close = &after_open[close_start + close_len..];

            match templates.get(&id) {
                Some(InlineTemplate::Paired { start, end }) => {
                    output.push(RenderedEvent::marker(
                        start.clone(),
                        MarkerEdge::Start(id.clone()),
                    ));
                    used.push(id.clone());
                    push_marked_fragment(inner, templates, output, used)?;
                    output.push(RenderedEvent::marker(
                        Event::End(end.clone()),
                        MarkerEdge::End(id),
                    ));
                }
                Some(InlineTemplate::Empty(_)) => {
                    return Err(BookforgeError::InvalidInput(format!(
                        "inline marker '{id}' was returned as paired markup but was empty in the source"
                    )));
                }
                None => {
                    return Err(BookforgeError::InvalidInput(format!(
                        "translation contains unknown inline marker '{id}'"
                    )));
                }
            }

            text = after_close;
        } else if let Some(empty) = parse_empty_marker(tag) {
            let id = empty.id;
            if used.iter().any(|seen| seen == &id) {
                return Err(BookforgeError::InvalidInput(format!(
                    "translation contains a duplicate formatting marker '{id}'. The LLM copied the marker twice."
                )));
            }

            match templates.get(&id) {
                Some(InlineTemplate::Empty(event)) => {
                    used.push(id.clone());
                    output.push(RenderedEvent::plain(event.clone()));
                }
                Some(InlineTemplate::Paired { .. }) => {
                    return Err(BookforgeError::InvalidInput(format!(
                        "inline marker '{id}' was returned as empty markup but was paired in the source"
                    )));
                }
                None => {
                    return Err(BookforgeError::InvalidInput(format!(
                        "translation contains unknown inline marker '{id}'"
                    )));
                }
            }

            text = &tag[empty.len..];
        } else {
            push_text_event("<", output);
            text = &tag[1..];
        }
    }

    push_text_event(text, output);
    Ok(())
}

fn push_text_event(text: &str, output: &mut Vec<RenderedEvent>) {
    if !text.is_empty() {
        output.push(RenderedEvent::plain(Event::Text(
            BytesText::new(text).into_owned(),
        )));
    }
}

fn find_matching_marker_close(text: &str, tag_name: &str) -> Result<usize> {
    let close = format!("</{tag_name}>");
    let mut depth = 0usize;
    let mut offset = 0usize;

    loop {
        let remaining = &text[offset..];
        let next_open = find_marker_open(remaining, tag_name);
        let next_close = remaining.find(&close);

        match (next_open, next_close) {
            (_, Some(close_index))
                if next_open.is_none_or(|open_index| close_index < open_index) =>
            {
                if depth == 0 {
                    return Ok(offset + close_index);
                }
                depth -= 1;
                offset += close_index + close.len();
            }
            (None, Some(close_index)) => {
                if depth == 0 {
                    return Ok(offset + close_index);
                }
                depth -= 1;
                offset += close_index + close.len();
            }
            (Some(open_index), _) => {
                let absolute = offset + open_index;
                let after_open = &text[absolute..];
                let Some(end) = after_open.find('>') else {
                    return Err(BookforgeError::InvalidInput(format!(
                        "inline marker '<{tag_name}>' is missing a closing '>'"
                    )));
                };
                depth += 1;
                offset = absolute + end + 1;
            }
            (None, None) => {
                return Err(BookforgeError::InvalidInput(format!(
                    "inline marker '<{tag_name}>' is missing closing tag '{close}'"
                )));
            }
        }
    }
}

fn find_marker_open(text: &str, tag_name: &str) -> Option<usize> {
    if matches!(tag_name, "m" | "keep") {
        text.find(&format!("<{tag_name} "))
    } else {
        text.find(&format!("<{tag_name}>"))
    }
}

fn marker_id(prefix: &str, marker_ordinal: usize) -> String {
    format!("{prefix}{}", marker_ordinal + 1)
}

pub(crate) fn validate_xml(xml: &str) -> Result<()> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    loop {
        match reader.read_event()? {
            Event::Eof => return Ok(()),
            _ => continue,
        }
    }
}

/// Consume one text-node index in the enclosing frame and return the
/// matching patch translation, if any. Whitespace-only nodes consume no
/// index, mirroring the reader's counting rule.
fn text_node_patch<'a>(
    patch_map: &HashMap<&[usize], PatchSpec<'a>>,
    stack: &mut [ElementFrame],
    non_whitespace: bool,
) -> Option<&'a str> {
    if !non_whitespace {
        return None;
    }
    let frame = stack.last_mut()?;
    let mut path = frame.path.clone();
    path.push(TEXT_NODE_PATH_BASE + frame.text_count);
    frame.text_count += 1;
    patch_map
        .get(path.as_slice())
        .map(|patch| patch.translation)
}

fn enter_element(stack: &mut Vec<ElementFrame>) -> Vec<usize> {
    let path = next_child_path(stack);
    stack.push(ElementFrame {
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

#[cfg(test)]
mod tests {
    use super::*;
    use bookforge_core::ir::{BlockId, BlockKind, InlineMark, ProtectedSpan, SectionId, TextRun};

    #[test]
    fn patch_opf_language_sets_target_language_tag() {
        let opf = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:language>en</dc:language>
  </metadata>
</package>"#;

        let patched = patch_opf_language(opf, "Italian").expect("language should patch");

        assert!(patched.contains("<dc:language>it</dc:language>"));
        validate_xml(&patched).expect("patched OPF should remain XML");
    }

    #[test]
    fn escapes_xml_special_characters_in_translation() {
        let xhtml = "<root><p>Original</p></root>";
        let path = DomPath(vec![0, 0]);
        let outcome =
            patch_xhtml(xhtml, &[(&path, "Tom & Jerry <think>")]).expect("patch should succeed");

        assert_eq!(outcome.skipped_blocks, 0);
        assert!(
            outcome.xhtml.contains("Tom &amp; Jerry &lt;think&gt;"),
            "expected escaped translation, got: {}",
            outcome.xhtml,
        );
        validate_xml(&outcome.xhtml).expect("escaped output should re-parse");
    }

    #[test]
    fn preserves_inline_children_and_skips_block() {
        let xhtml = "<root><p>Hello <em>world</em>!</p></root>";
        let path = DomPath(vec![0, 0]);
        let outcome = patch_xhtml(xhtml, &[(&path, "Ciao mondo!")]).expect("patch should succeed");

        assert_eq!(outcome.skipped_blocks, 1);
        assert!(
            outcome.xhtml.contains("<em>world</em>"),
            "inline child must survive, got: {}",
            outcome.xhtml,
        );
        assert!(
            outcome.xhtml.contains("Hello "),
            "original text must survive when block is skipped, got: {}",
            outcome.xhtml,
        );
        assert!(
            !outcome.xhtml.contains("Ciao mondo!"),
            "translation must not be applied when inline children are present, got: {}",
            outcome.xhtml,
        );
        validate_xml(&outcome.xhtml).expect("preserved output should re-parse");
    }

    #[test]
    fn applies_marker_translation_to_inline_children() {
        let xhtml = "<root><p>Hello <em>world</em>!</p></root>";
        let block = block(
            "b_000000",
            DomPath(vec![0, 0]),
            vec![InlineMark {
                id: "m1".to_string(),
                kind: "em".to_string(),
            }],
        );
        let outcome = patch_xhtml_blocks(
            xhtml,
            &[BlockPatch {
                block: &block,
                translation: "Ciao <m1>mondo</m1>!",
            }],
        )
        .expect("patch should succeed");

        assert_eq!(outcome.skipped_blocks, 0);
        assert!(
            outcome.xhtml.contains("<em>mondo</em>"),
            "inline child should be translated through marker, got: {}",
            outcome.xhtml,
        );
        assert!(
            !outcome.xhtml.contains("world"),
            "original inline text should be replaced, got: {}",
            outcome.xhtml,
        );
        validate_xml(&outcome.xhtml).expect("marked output should re-parse");
    }

    #[test]
    fn restores_space_between_adjacent_span_markers() {
        let xhtml = "<root><p><span>a</span> <span>b</span></p></root>";
        let block = block(
            "b_000000",
            DomPath(vec![0, 0]),
            vec![
                InlineMark {
                    id: "m1".to_string(),
                    kind: "span".to_string(),
                },
                InlineMark {
                    id: "m2".to_string(),
                    kind: "span".to_string(),
                },
            ],
        );
        let outcome = patch_xhtml_blocks(
            xhtml,
            &[BlockPatch {
                block: &block,
                translation: "<m1>verso</m1><m2>Thanatos</m2>",
            }],
        )
        .expect("patch should succeed");

        assert_eq!(outcome.skipped_blocks, 0);
        assert!(
            outcome
                .xhtml
                .contains("<span>verso</span> <span>Thanatos</span>"),
            "writer should restore the source inter-span space, got: {}",
            outcome.xhtml,
        );
        validate_xml(&outcome.xhtml).expect("marked output should re-parse");
    }

    #[test]
    fn restores_space_between_adjacent_italic_markers() {
        let xhtml = "<root><p><i>a</i> <i>b</i></p></root>";
        let block = block(
            "b_000000",
            DomPath(vec![0, 0]),
            vec![
                InlineMark {
                    id: "m1".to_string(),
                    kind: "i".to_string(),
                },
                InlineMark {
                    id: "m2".to_string(),
                    kind: "i".to_string(),
                },
            ],
        );
        let outcome = patch_xhtml_blocks(
            xhtml,
            &[BlockPatch {
                block: &block,
                translation: "<m1>a</m1><m2>b</m2>",
            }],
        )
        .expect("patch should succeed");

        assert_eq!(outcome.skipped_blocks, 0);
        assert!(
            outcome.xhtml.contains("<i>a</i> <i>b</i>"),
            "writer should restore the source inter-italic space, got: {}",
            outcome.xhtml,
        );
        validate_xml(&outcome.xhtml).expect("marked output should re-parse");
    }

    #[test]
    fn restores_space_for_nbsp_only_inline_boundary() {
        let xhtml = "<root><p><span>a</span>&nbsp;<span>b</span></p></root>";
        let block = block(
            "b_000000",
            DomPath(vec![0, 0]),
            vec![
                InlineMark {
                    id: "m1".to_string(),
                    kind: "span".to_string(),
                },
                InlineMark {
                    id: "m2".to_string(),
                    kind: "span".to_string(),
                },
            ],
        );
        let outcome = patch_xhtml_blocks(
            xhtml,
            &[BlockPatch {
                block: &block,
                translation: "<m1>a</m1><m2>b</m2>",
            }],
        )
        .expect("patch should succeed");

        assert_eq!(outcome.skipped_blocks, 0);
        assert!(
            outcome.xhtml.contains("<span>a</span> <span>b</span>"),
            "writer should restore the source non-breaking inter-span boundary, got: {}",
            outcome.xhtml,
        );
        validate_xml(&outcome.xhtml).expect("marked output should re-parse");
    }

    #[test]
    fn replaces_text_only_block_with_translation() {
        let xhtml = "<root><p>Original</p><p>Other</p></root>";
        let first = DomPath(vec![0, 0]);
        let outcome = patch_xhtml(xhtml, &[(&first, "Tradotto")]).expect("patch should succeed");

        assert_eq!(outcome.skipped_blocks, 0);
        assert!(outcome.xhtml.contains("<p>Tradotto</p>"));
        assert!(
            outcome.xhtml.contains("<p>Other</p>"),
            "untargeted block must be untouched"
        );
        validate_xml(&outcome.xhtml).expect("output should re-parse");
    }

    #[test]
    fn patches_stray_text_node() {
        let xhtml = "<root><p>Para</p>tail text<p>Other</p></root>";
        let path = DomPath(vec![0, TEXT_NODE_PATH_BASE]);
        let outcome =
            patch_xhtml(xhtml, &[(&path, "coda tradotta")]).expect("patch should succeed");

        assert_eq!(outcome.skipped_blocks, 0);
        assert!(
            outcome.xhtml.contains("coda tradotta"),
            "stray text node should be replaced, got: {}",
            outcome.xhtml,
        );
        assert!(!outcome.xhtml.contains("tail text"));
        assert!(
            outcome.xhtml.contains("<p>Para</p>") && outcome.xhtml.contains("<p>Other</p>"),
            "sibling elements must be untouched, got: {}",
            outcome.xhtml,
        );
        validate_xml(&outcome.xhtml).expect("output should re-parse");
    }

    #[test]
    fn validate_xml_rejects_malformed_input() {
        assert!(validate_xml("<root><p>oops</root>").is_err());
    }

    fn block(id: &str, dom_path: DomPath, inline_marks: Vec<InlineMark>) -> Block {
        Block {
            id: BlockId(id.to_string()),
            section_id: SectionId("sec_000000".to_string()),
            kind: BlockKind::Paragraph,
            dom_path,
            text_runs: vec![TextRun {
                id: "r000000_000".to_string(),
                text: "Hello <m1>world</m1>!".to_string(),
            }],
            inline_marks,
            protected_spans: Vec::<ProtectedSpan>::new(),
            token_estimate: 4,
        }
    }
}
