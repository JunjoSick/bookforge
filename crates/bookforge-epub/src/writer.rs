use std::{
    collections::HashMap,
    fs::File,
    io::{Read, Write},
    path::Path,
};

use bookforge_core::{
    BookforgeError, Result,
    ir::{Block, Book, DomPath, TEXT_NODE_PATH_BASE},
    marker::extract_marker_id,
    segment::BlockTranslation,
};
use quick_xml::{
    Reader, Writer,
    events::{BytesEnd, BytesText, Event},
};
use zip::{CompressionMethod, DateTime, ZipArchive, ZipWriter, write::SimpleFileOptions};

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
            validate_xml(&outcome.xhtml).map_err(|err| {
                BookforgeError::InvalidInput(format!(
                    "patched XHTML '{name}' failed validation: {err}"
                ))
            })?;
            outcome.xhtml.into_bytes()
        } else {
            bytes
        };

        writer.start_file(name, deflated)?;
        writer.write_all(&output_bytes)?;
    }

    writer.finish()?;

    if total_skipped > 0 {
        tracing::warn!(
            skipped_blocks = total_skipped,
            "rebuild left {total_skipped} block(s) untranslated to preserve inline structure"
        );
    }

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

    Ok(rendered)
}

fn collect_inline_templates(
    block: &Block,
    events: &[Event<'static>],
) -> Result<HashMap<String, InlineTemplate>> {
    let block_ordinal = block_ordinal(block)?;
    let mut marker_ordinal = 0usize;
    let mut templates = HashMap::new();
    let mut stack = Vec::<(String, Event<'static>)>::new();

    for event in events {
        match event {
            Event::Start(element) => {
                let id = marker_id("m", block_ordinal, marker_ordinal);
                marker_ordinal += 1;
                stack.push((id, Event::Start(element.clone())));
            }
            Event::Empty(element) => {
                let id = marker_id("r", block_ordinal, marker_ordinal);
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

fn push_marked_fragment(
    mut text: &str,
    templates: &HashMap<String, InlineTemplate>,
    output: &mut Vec<Event<'static>>,
    used: &mut Vec<String>,
) -> Result<()> {
    while let Some(index) = text.find('<') {
        push_text_event(&text[..index], output);
        let tag = &text[index..];

        if let Some((tag_name, id, open_len)) = parse_paired_marker_open(tag) {
            if used.iter().any(|seen| seen == &id) {
                return Err(BookforgeError::InvalidInput(format!(
                    "translation contains a duplicate formatting marker '{id}'. The LLM copied the marker twice."
                )));
            }

            let after_open = &tag[open_len..];
            let close_start = find_matching_marker_close(after_open, tag_name)?;
            let close_len = format!("</{tag_name}>").len();
            let inner = &after_open[..close_start];
            let after_close = &after_open[close_start + close_len..];

            match templates.get(&id) {
                Some(InlineTemplate::Paired { start, end }) => {
                    output.push(start.clone());
                    used.push(id);
                    push_marked_fragment(inner, templates, output, used)?;
                    output.push(Event::End(end.clone()));
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
        } else if let Some((id, marker_len)) = parse_empty_marker(tag) {
            if used.iter().any(|seen| seen == &id) {
                return Err(BookforgeError::InvalidInput(format!(
                    "translation contains a duplicate formatting marker '{id}'. The LLM copied the marker twice."
                )));
            }

            match templates.get(&id) {
                Some(InlineTemplate::Empty(event)) => {
                    used.push(id);
                    output.push(event.clone());
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

            text = &tag[marker_len..];
        } else {
            push_text_event("<", output);
            text = &tag[1..];
        }
    }

    push_text_event(text, output);
    Ok(())
}

fn push_text_event(text: &str, output: &mut Vec<Event<'static>>) {
    if !text.is_empty() {
        output.push(Event::Text(BytesText::new(text).into_owned()));
    }
}

fn parse_paired_marker_open(text: &str) -> Option<(&'static str, String, usize)> {
    for tag_name in ["m", "keep"] {
        let prefix = format!("<{tag_name} ");
        if !text.starts_with(&prefix) {
            continue;
        }
        let open_end = text.find('>')?;
        if text[..open_end].ends_with('/') {
            return None;
        }
        let id = extract_marker_id(&text[..=open_end])?;
        return Some((tag_name, id, open_end + 1));
    }
    None
}

fn parse_empty_marker(text: &str) -> Option<(String, usize)> {
    for tag_name in ["ref", "m", "keep"] {
        if !text.starts_with(&format!("<{tag_name} ")) {
            continue;
        }
        if let Some(end) = text.find("/>") {
            let tag = &text[..end + 2];
            if let Some(id) = extract_marker_id(tag) {
                return Some((id, end + 2));
            }
        }
    }
    None
}

fn find_matching_marker_close(text: &str, tag_name: &str) -> Result<usize> {
    let open_prefix = format!("<{tag_name} ");
    let close = format!("</{tag_name}>");
    let mut depth = 0usize;
    let mut offset = 0usize;

    loop {
        let remaining = &text[offset..];
        let next_open = remaining.find(&open_prefix);
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

fn block_ordinal(block: &Block) -> Result<usize> {
    block
        .id
        .0
        .strip_prefix("b_")
        .and_then(|value| value.parse::<usize>().ok())
        .ok_or_else(|| {
            BookforgeError::InvalidInput(format!(
                "block id '{}' does not use the expected b_000000 shape",
                block.id.0
            ))
        })
}

fn marker_id(prefix: &str, block_ordinal: usize, marker_ordinal: usize) -> String {
    format!("{prefix}{block_ordinal:06}_{marker_ordinal:03}")
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
                id: "m000000_000".to_string(),
                kind: "em".to_string(),
            }],
        );
        let outcome = patch_xhtml_blocks(
            xhtml,
            &[BlockPatch {
                block: &block,
                translation: "Ciao <m id=\"m000000_000\">mondo</m>!",
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
                text: "Hello <m id=\"m000000_000\">world</m>!".to_string(),
            }],
            inline_marks,
            protected_spans: Vec::<ProtectedSpan>::new(),
            token_estimate: 4,
        }
    }
}
