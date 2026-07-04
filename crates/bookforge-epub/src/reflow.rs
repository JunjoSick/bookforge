use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use bookforge_core::{BookforgeError, Result};
use quick_xml::{
    Reader, Writer,
    events::{BytesCData, BytesEnd, BytesStart, BytesText, Event},
};
use serde::Serialize;
use zip::{CompressionMethod, DateTime, ZipArchive, ZipWriter, write::SimpleFileOptions};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReflowOptions {
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflowOutcome {
    pub report: ReflowReport,
    pub output_written: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReflowReport {
    pub schema_version: u32,
    pub input_path: String,
    pub output_path: String,
    pub totals: ReflowTotals,
    pub merges: Vec<ReflowMergeRecord>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ReflowTotals {
    pub files_checked: usize,
    pub files_touched: usize,
    pub paragraphs_before: usize,
    pub paragraphs_after: usize,
    pub merge_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReflowMergeRecord {
    pub resource: String,
    pub block_index: usize,
    pub merged_block_index: usize,
    pub left_preview: String,
    pub right_preview: String,
    pub dehyphenated: bool,
}

pub fn reflow_epub(input: &Path, output: &Path, options: &ReflowOptions) -> Result<ReflowOutcome> {
    let result = if options.dry_run {
        write_reflowed_epub(input, output, None)
    } else {
        let staged = sibling_work_path(output, "reflow");
        let result = with_output_writer(input, output, &staged);
        match result {
            Ok(report) => {
                if let Err(error) = commit_staged_output(&staged, output) {
                    let _ = fs::remove_file(&staged);
                    return Err(error);
                }
                Ok(report)
            }
            Err(error) => {
                let _ = fs::remove_file(&staged);
                Err(error)
            }
        }
    }?;

    Ok(ReflowOutcome {
        report: result,
        output_written: !options.dry_run,
    })
}

fn with_output_writer(input: &Path, output: &Path, staged: &Path) -> Result<ReflowReport> {
    let output_file = File::create(staged)?;
    let writer = ZipWriter::new(output_file);
    write_reflowed_epub(input, output, Some(writer))
}

fn write_reflowed_epub(
    input: &Path,
    output: &Path,
    writer: Option<ZipWriter<File>>,
) -> Result<ReflowReport> {
    let source = File::open(input)?;
    let mut archive = ZipArchive::new(source)?;
    let mut report = ReflowReport {
        schema_version: 1,
        input_path: input.display().to_string(),
        output_path: output.display().to_string(),
        totals: ReflowTotals::default(),
        merges: Vec::new(),
    };

    match writer {
        Some(mut writer) => {
            write_mimetype_first(&mut archive, &mut writer)?;
            write_archive_entries(&mut archive, Some(&mut writer), &mut report)?;
            writer.finish()?;
        }
        None => {
            validate_mimetype(&mut archive)?;
            write_archive_entries(&mut archive, None, &mut report)?;
        }
    }

    Ok(report)
}

fn write_archive_entries(
    archive: &mut ZipArchive<File>,
    mut writer: Option<&mut ZipWriter<File>>,
    report: &mut ReflowReport,
) -> Result<()> {
    let deflated = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .last_modified_time(deterministic_zip_time());

    for index in 0..archive.len() {
        let mut file = archive.by_index(index)?;
        let name = file.name().to_string();

        if name == "mimetype" {
            continue;
        }

        if file.is_dir() {
            if let Some(writer) = writer.as_deref_mut() {
                writer.add_directory(name, deflated)?;
            }
            continue;
        }

        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let output_bytes = if is_xhtml_resource_name(&name) {
            let xhtml = String::from_utf8(bytes).map_err(|err| {
                BookforgeError::InvalidInput(format!("XHTML resource '{name}' is not UTF-8: {err}"))
            })?;
            let outcome = reflow_xhtml_resource(&xhtml, &name)?;
            report.totals.files_checked += 1;
            report.totals.paragraphs_before += outcome.paragraphs_before;
            report.totals.paragraphs_after += outcome.paragraphs_after;
            report.totals.merge_count += outcome.merges.len();
            if !outcome.merges.is_empty() {
                report.totals.files_touched += 1;
                report.merges.extend(outcome.merges);
                outcome.xhtml.into_bytes()
            } else {
                xhtml.into_bytes()
            }
        } else {
            bytes
        };

        if let Some(writer) = writer.as_deref_mut() {
            writer.start_file(name, deflated)?;
            writer.write_all(&output_bytes)?;
        }
    }

    Ok(())
}

#[derive(Debug)]
struct ResourceReflow {
    xhtml: String,
    paragraphs_before: usize,
    paragraphs_after: usize,
    merges: Vec<ReflowMergeRecord>,
}

fn reflow_xhtml_resource(xhtml: &str, resource: &str) -> Result<ResourceReflow> {
    let (mut nodes, paragraphs_before) = parse_xml(xhtml)?;
    let mut merges = Vec::new();
    reflow_nodes(&mut nodes, resource, &mut merges)?;
    let paragraphs_after = count_paragraphs(&nodes);

    if merges.is_empty() {
        return Ok(ResourceReflow {
            xhtml: xhtml.to_string(),
            paragraphs_before,
            paragraphs_after,
            merges,
        });
    }

    let reflowed = write_xml(&nodes)?;
    validate_xml(&reflowed).map_err(|err| {
        BookforgeError::InvalidInput(format!(
            "reflowed XHTML '{resource}' failed validation: {err}"
        ))
    })?;

    Ok(ResourceReflow {
        xhtml: reflowed,
        paragraphs_before,
        paragraphs_after,
        merges,
    })
}

#[derive(Debug, Clone)]
enum XmlNode {
    Element(XmlElement),
    Empty { start: BytesStart<'static> },
    Leaf(Event<'static>),
}

#[derive(Debug, Clone)]
struct XmlElement {
    start: BytesStart<'static>,
    children: Vec<XmlNode>,
    end: BytesEnd<'static>,
    paragraph_index: Option<usize>,
}

#[derive(Debug)]
struct BuildingElement {
    start: BytesStart<'static>,
    children: Vec<XmlNode>,
    paragraph_index: Option<usize>,
}

fn parse_xml(xml: &str) -> Result<(Vec<XmlNode>, usize)> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut roots = Vec::new();
    let mut stack = Vec::<BuildingElement>::new();
    let mut paragraph_count = 0usize;

    loop {
        match reader.read_event()? {
            Event::Start(element) => {
                let start = element.into_owned();
                let paragraph_index = paragraph_index_for(&start, &mut paragraph_count);
                stack.push(BuildingElement {
                    start,
                    children: Vec::new(),
                    paragraph_index,
                });
            }
            Event::Empty(element) => {
                let start = element.into_owned();
                let _ = paragraph_index_for(&start, &mut paragraph_count);
                push_node(&mut roots, &mut stack, XmlNode::Empty { start });
            }
            Event::End(end) => {
                let Some(frame) = stack.pop() else {
                    return Err(BookforgeError::InvalidInput(
                        "unexpected closing tag while parsing XHTML".to_string(),
                    ));
                };
                push_node(
                    &mut roots,
                    &mut stack,
                    XmlNode::Element(XmlElement {
                        start: frame.start,
                        children: frame.children,
                        end: end.into_owned(),
                        paragraph_index: frame.paragraph_index,
                    }),
                );
            }
            Event::Eof => break,
            event => push_node(&mut roots, &mut stack, XmlNode::Leaf(event.into_owned())),
        }
    }

    if !stack.is_empty() {
        return Err(BookforgeError::InvalidInput(
            "unexpected end of XHTML while parsing element tree".to_string(),
        ));
    }

    Ok((roots, paragraph_count))
}

fn paragraph_index_for(start: &BytesStart<'_>, paragraph_count: &mut usize) -> Option<usize> {
    if local_name(start.name().as_ref()) == b"p" {
        let index = *paragraph_count;
        *paragraph_count += 1;
        Some(index)
    } else {
        None
    }
}

fn push_node(roots: &mut Vec<XmlNode>, stack: &mut [BuildingElement], node: XmlNode) {
    if let Some(parent) = stack.last_mut() {
        parent.children.push(node);
    } else {
        roots.push(node);
    }
}

fn write_xml(nodes: &[XmlNode]) -> Result<String> {
    let mut writer = Writer::new(Vec::new());
    write_nodes(&mut writer, nodes)?;
    String::from_utf8(writer.into_inner()).map_err(|err| {
        BookforgeError::InvalidInput(format!("reflowed XHTML is not valid UTF-8: {err}"))
    })
}

fn write_nodes(writer: &mut Writer<Vec<u8>>, nodes: &[XmlNode]) -> Result<()> {
    for node in nodes {
        write_node(writer, node)?;
    }
    Ok(())
}

fn write_node(writer: &mut Writer<Vec<u8>>, node: &XmlNode) -> Result<()> {
    match node {
        XmlNode::Element(element) => {
            writer.write_event(Event::Start(element.start.borrow()))?;
            write_nodes(writer, &element.children)?;
            writer.write_event(Event::End(element.end.borrow()))?;
        }
        XmlNode::Empty { start } => {
            writer.write_event(Event::Empty(start.borrow()))?;
        }
        XmlNode::Leaf(event) => writer.write_event(event.borrow())?,
    }
    Ok(())
}

fn reflow_nodes(
    nodes: &mut Vec<XmlNode>,
    resource: &str,
    merges: &mut Vec<ReflowMergeRecord>,
) -> Result<()> {
    for node in nodes.iter_mut() {
        if let XmlNode::Element(element) = node {
            reflow_nodes(&mut element.children, resource, merges)?;
        }
    }

    let mut left_index = 0usize;
    while left_index < nodes.len() {
        if !is_paragraph_element(&nodes[left_index]) {
            left_index += 1;
            continue;
        }

        let Some(right_index) = next_paragraph_after_whitespace(nodes, left_index + 1)? else {
            left_index += 1;
            continue;
        };

        let decision = match (&nodes[left_index], &nodes[right_index]) {
            (XmlNode::Element(left), XmlNode::Element(right)) => merge_decision(left, right)?,
            _ => None,
        };

        let Some(decision) = decision else {
            left_index += 1;
            continue;
        };

        let mut drained = nodes
            .drain(left_index + 1..=right_index)
            .collect::<Vec<_>>();
        let right = drained.pop().expect("right paragraph should be drained");
        let XmlNode::Element(mut right) = right else {
            unreachable!("merge decision only accepts element paragraphs")
        };
        let XmlNode::Element(left) = &mut nodes[left_index] else {
            unreachable!("merge decision only accepts element paragraphs")
        };

        append_merged_children(left, &mut right, decision.dehyphenated)?;
        merges.push(decision.into_record(resource));
    }

    Ok(())
}

fn next_paragraph_after_whitespace(nodes: &[XmlNode], start: usize) -> Result<Option<usize>> {
    let mut index = start;
    while index < nodes.len() {
        if is_paragraph_element(&nodes[index]) {
            return Ok(Some(index));
        }
        if !is_whitespace_node(&nodes[index])? {
            return Ok(None);
        }
        index += 1;
    }
    Ok(None)
}

#[derive(Debug)]
struct MergeDecision {
    block_index: usize,
    merged_block_index: usize,
    left_preview: String,
    right_preview: String,
    dehyphenated: bool,
}

impl MergeDecision {
    fn into_record(self, resource: &str) -> ReflowMergeRecord {
        ReflowMergeRecord {
            resource: resource.to_string(),
            block_index: self.block_index,
            merged_block_index: self.merged_block_index,
            left_preview: self.left_preview,
            right_preview: self.right_preview,
            dehyphenated: self.dehyphenated,
        }
    }
}

fn merge_decision(left: &XmlElement, right: &XmlElement) -> Result<Option<MergeDecision>> {
    let left_text = visible_text(&left.children)?;
    let right_text = visible_text(&right.children)?;

    if left_text.trim().is_empty() || right_text.trim().is_empty() {
        return Ok(None);
    }
    if ends_with_terminal_punctuation(&left_text) {
        return Ok(None);
    }
    if !starts_with_unicode_lowercase(&right_text) {
        return Ok(None);
    }
    if attr_value_unescaped(&left.start, b"class")? != attr_value_unescaped(&right.start, b"class")?
    {
        return Ok(None);
    }
    if attr_value_unescaped(&right.start, b"id")?.is_some() {
        return Ok(None);
    }
    if contains_nested_block_or_replaced(&left.children)
        || contains_nested_block_or_replaced(&right.children)
    {
        return Ok(None);
    }

    Ok(Some(MergeDecision {
        block_index: left.paragraph_index.unwrap_or_default(),
        merged_block_index: right.paragraph_index.unwrap_or_default(),
        left_preview: preview(&left_text),
        right_preview: preview(&right_text),
        dehyphenated: should_dehyphenate(&left_text),
    }))
}

fn append_merged_children(
    left: &mut XmlElement,
    right: &mut XmlElement,
    dehyphenated: bool,
) -> Result<()> {
    if dehyphenated {
        trim_trailing_whitespace(&mut left.children)?;
        trim_leading_whitespace(&mut right.children)?;
        let _ = remove_trailing_hyphen(&mut left.children)?;
    } else {
        trim_trailing_whitespace(&mut left.children)?;
        trim_leading_whitespace(&mut right.children)?;
        left.children
            .push(XmlNode::Leaf(Event::Text(BytesText::new(" ").into_owned())));
    }
    left.children.append(&mut right.children);
    Ok(())
}

fn is_paragraph_element(node: &XmlNode) -> bool {
    matches!(node, XmlNode::Element(element) if local_name(element.start.name().as_ref()) == b"p")
}

fn count_paragraphs(nodes: &[XmlNode]) -> usize {
    nodes.iter().map(count_paragraphs_in_node).sum()
}

fn count_paragraphs_in_node(node: &XmlNode) -> usize {
    match node {
        XmlNode::Element(element) => {
            usize::from(local_name(element.start.name().as_ref()) == b"p")
                + count_paragraphs(&element.children)
        }
        XmlNode::Empty { start } => usize::from(local_name(start.name().as_ref()) == b"p"),
        XmlNode::Leaf(_) => 0,
    }
}

fn visible_text(nodes: &[XmlNode]) -> Result<String> {
    let mut text = String::new();
    append_visible_text(nodes, &mut text)?;
    Ok(normalize_space(&text))
}

fn append_visible_text(nodes: &[XmlNode], text: &mut String) -> Result<()> {
    for node in nodes {
        match node {
            XmlNode::Element(element) => append_visible_text(&element.children, text)?,
            XmlNode::Empty { .. } => {}
            XmlNode::Leaf(event) => append_event_text(event, text)?,
        }
    }
    Ok(())
}

fn append_event_text(event: &Event<'static>, text: &mut String) -> Result<()> {
    match event {
        Event::Text(value) => {
            text.push_str(
                &value
                    .html_content()
                    .map_err(|err| BookforgeError::InvalidInput(err.to_string()))?,
            );
        }
        Event::CData(value) => {
            text.push_str(
                &value
                    .decode()
                    .map_err(|err| BookforgeError::InvalidInput(err.to_string()))?,
            );
        }
        Event::GeneralRef(reference) => {
            if let Some(value) = resolve_general_ref(reference)? {
                text.push_str(&value);
            }
        }
        _ => {}
    }
    Ok(())
}

fn is_whitespace_node(node: &XmlNode) -> Result<bool> {
    match node {
        XmlNode::Leaf(Event::Text(text)) => Ok(text
            .html_content()
            .map_err(|err| BookforgeError::InvalidInput(err.to_string()))?
            .chars()
            .all(char::is_whitespace)),
        XmlNode::Leaf(Event::CData(text)) => Ok(text
            .decode()
            .map_err(|err| BookforgeError::InvalidInput(err.to_string()))?
            .chars()
            .all(char::is_whitespace)),
        XmlNode::Leaf(Event::GeneralRef(reference)) => Ok(resolve_general_ref(reference)?
            .is_some_and(|value| value.chars().all(char::is_whitespace))),
        _ => Ok(false),
    }
}

fn contains_nested_block_or_replaced(nodes: &[XmlNode]) -> bool {
    nodes.iter().any(|node| match node {
        XmlNode::Element(element) => {
            let raw_name = element.start.name();
            let name = local_name(raw_name.as_ref());
            is_block_level_name(name)
                || is_replaced_content_name(name)
                || contains_nested_block_or_replaced(&element.children)
        }
        XmlNode::Empty { start } => {
            let raw_name = start.name();
            let name = local_name(raw_name.as_ref());
            is_block_level_name(name) || is_replaced_content_name(name)
        }
        XmlNode::Leaf(_) => false,
    })
}

fn ends_with_terminal_punctuation(text: &str) -> bool {
    for ch in text.trim_end().chars().rev() {
        if is_terminal_closer(ch) {
            continue;
        }
        return is_terminal_punctuation(ch);
    }
    false
}

fn is_terminal_punctuation(ch: char) -> bool {
    matches!(ch, '.' | '!' | '?' | ':' | ';' | '…')
}

fn is_terminal_closer(ch: char) -> bool {
    matches!(ch, '"' | '”' | '’' | '»' | ')' | ']')
}

fn starts_with_unicode_lowercase(text: &str) -> bool {
    text.trim_start()
        .chars()
        .next()
        .is_some_and(char::is_lowercase)
}

fn should_dehyphenate(text: &str) -> bool {
    let mut chars = text.trim_end().chars().rev();
    if chars.next() != Some('-') {
        return false;
    }
    chars.next().is_some_and(is_word_char)
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

fn trim_trailing_whitespace(nodes: &mut Vec<XmlNode>) -> Result<()> {
    while let Some(last) = nodes.last_mut() {
        match trim_trailing_whitespace_node(last)? {
            TrimResult::RemoveNode => {
                nodes.pop();
            }
            TrimResult::Done => return Ok(()),
        }
    }
    Ok(())
}

fn trim_trailing_whitespace_node(node: &mut XmlNode) -> Result<TrimResult> {
    match node {
        XmlNode::Element(element) => {
            trim_trailing_whitespace(&mut element.children)?;
            Ok(TrimResult::Done)
        }
        XmlNode::Leaf(Event::Text(text)) => {
            let value = text
                .html_content()
                .map_err(|err| BookforgeError::InvalidInput(err.to_string()))?;
            let trimmed = value.trim_end();
            if trimmed.is_empty() {
                Ok(TrimResult::RemoveNode)
            } else if trimmed.len() == value.len() {
                Ok(TrimResult::Done)
            } else {
                *text = BytesText::new(trimmed).into_owned();
                Ok(TrimResult::Done)
            }
        }
        XmlNode::Leaf(Event::CData(text)) => {
            let value = text
                .decode()
                .map_err(|err| BookforgeError::InvalidInput(err.to_string()))?;
            let trimmed = value.trim_end();
            if trimmed.is_empty() {
                Ok(TrimResult::RemoveNode)
            } else if trimmed.len() == value.len() {
                Ok(TrimResult::Done)
            } else {
                *text = BytesCData::new(trimmed).into_owned();
                Ok(TrimResult::Done)
            }
        }
        _ => Ok(TrimResult::Done),
    }
}

fn trim_leading_whitespace(nodes: &mut Vec<XmlNode>) -> Result<()> {
    loop {
        let Some(first) = nodes.first_mut() else {
            return Ok(());
        };
        match trim_leading_whitespace_node(first)? {
            TrimResult::RemoveNode => {
                nodes.remove(0);
            }
            TrimResult::Done => return Ok(()),
        }
    }
}

fn trim_leading_whitespace_node(node: &mut XmlNode) -> Result<TrimResult> {
    match node {
        XmlNode::Element(element) => {
            trim_leading_whitespace(&mut element.children)?;
            Ok(TrimResult::Done)
        }
        XmlNode::Leaf(Event::Text(text)) => {
            let value = text
                .html_content()
                .map_err(|err| BookforgeError::InvalidInput(err.to_string()))?;
            let trimmed = value.trim_start();
            if trimmed.is_empty() {
                Ok(TrimResult::RemoveNode)
            } else if trimmed.len() == value.len() {
                Ok(TrimResult::Done)
            } else {
                *text = BytesText::new(trimmed).into_owned();
                Ok(TrimResult::Done)
            }
        }
        XmlNode::Leaf(Event::CData(text)) => {
            let value = text
                .decode()
                .map_err(|err| BookforgeError::InvalidInput(err.to_string()))?;
            let trimmed = value.trim_start();
            if trimmed.is_empty() {
                Ok(TrimResult::RemoveNode)
            } else if trimmed.len() == value.len() {
                Ok(TrimResult::Done)
            } else {
                *text = BytesCData::new(trimmed).into_owned();
                Ok(TrimResult::Done)
            }
        }
        _ => Ok(TrimResult::Done),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrimResult {
    RemoveNode,
    Done,
}

fn remove_trailing_hyphen(nodes: &mut [XmlNode]) -> Result<bool> {
    for node in nodes.iter_mut().rev() {
        match node {
            XmlNode::Element(element) => {
                if remove_trailing_hyphen(&mut element.children)? {
                    return Ok(true);
                }
                if !visible_text(&element.children)?.trim().is_empty() {
                    return Ok(false);
                }
            }
            XmlNode::Leaf(Event::Text(text)) => {
                let value = text
                    .html_content()
                    .map_err(|err| BookforgeError::InvalidInput(err.to_string()))?;
                if value.trim().is_empty() {
                    continue;
                }
                if let Some(stripped) = strip_trailing_hyphen(&value) {
                    *text = BytesText::new(&stripped).into_owned();
                    return Ok(true);
                }
                return Ok(false);
            }
            XmlNode::Leaf(Event::CData(text)) => {
                let value = text
                    .decode()
                    .map_err(|err| BookforgeError::InvalidInput(err.to_string()))?;
                if value.trim().is_empty() {
                    continue;
                }
                if let Some(stripped) = strip_trailing_hyphen(&value) {
                    *text = BytesCData::new(&stripped).into_owned();
                    return Ok(true);
                }
                return Ok(false);
            }
            XmlNode::Leaf(Event::GeneralRef(reference)) => {
                if resolve_general_ref(reference)?
                    .is_some_and(|value| value.chars().all(char::is_whitespace))
                {
                    continue;
                }
                return Ok(false);
            }
            XmlNode::Empty { .. } | XmlNode::Leaf(_) => return Ok(false),
        }
    }
    Ok(false)
}

fn strip_trailing_hyphen(text: &str) -> Option<String> {
    let trimmed = text.trim_end();
    let mut chars = trimmed.char_indices().rev();
    let (hyphen_index, hyphen) = chars.next()?;
    if hyphen != '-' {
        return None;
    }
    if !chars.next().is_some_and(|(_, ch)| is_word_char(ch)) {
        return None;
    }
    Some(text[..hyphen_index].to_string())
}

fn preview(text: &str) -> String {
    let text = normalize_space(text);
    let mut preview = text.chars().take(40).collect::<String>();
    if text.chars().count() > 40 {
        preview.push_str("...");
    }
    preview
}

fn normalize_space(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_block_level_name(name: &[u8]) -> bool {
    matches!(
        name,
        b"p" | b"div"
            | b"blockquote"
            | b"li"
            | b"ul"
            | b"ol"
            | b"dl"
            | b"dt"
            | b"dd"
            | b"table"
            | b"thead"
            | b"tbody"
            | b"tfoot"
            | b"tr"
            | b"td"
            | b"th"
            | b"caption"
            | b"h1"
            | b"h2"
            | b"h3"
            | b"h4"
            | b"h5"
            | b"h6"
            | b"section"
            | b"article"
            | b"aside"
            | b"figure"
            | b"figcaption"
            | b"pre"
            | b"hr"
    )
}

fn is_replaced_content_name(name: &[u8]) -> bool {
    matches!(
        name,
        b"img"
            | b"image"
            | b"svg"
            | b"math"
            | b"object"
            | b"embed"
            | b"iframe"
            | b"video"
            | b"audio"
            | b"canvas"
            | b"picture"
            | b"source"
    )
}

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

fn validate_xml(xml: &str) -> Result<()> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    loop {
        match reader.read_event() {
            Ok(Event::Eof) => return Ok(()),
            Ok(_) => continue,
            Err(error) => return Err(BookforgeError::InvalidInput(error.to_string())),
        }
    }
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
                    "reflowed EPUB is committed but its backup could not be removed"
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

fn is_xhtml_resource_name(name: &str) -> bool {
    name.rsplit_once('.')
        .map(|(_, extension)| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "xhtml" | "html" | "htm"
            )
        })
        .unwrap_or(false)
}

fn local_name(name: &[u8]) -> &[u8] {
    name.rsplit(|byte| *byte == b':').next().unwrap_or(name)
}

fn attr_value_unescaped(element: &BytesStart<'_>, attr_name: &[u8]) -> Result<Option<String>> {
    for attr in element.attributes() {
        let attr = attr.map_err(|err| BookforgeError::InvalidInput(err.to_string()))?;
        if local_name(attr.key.as_ref()) == attr_name {
            return Ok(Some(attr.unescape_value()?.into_owned()));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs::File,
        io::{Read, Write},
    };
    use zip::{ZipWriter, write::SimpleFileOptions};

    fn reflow_snippet(body: &str) -> ResourceReflow {
        let xhtml = format!("<html><body>{body}</body></html>");
        reflow_xhtml_resource(&xhtml, "chapter.xhtml").expect("snippet should reflow")
    }

    fn merge_count(body: &str) -> usize {
        reflow_snippet(body).merges.len()
    }

    #[test]
    fn merges_consecutive_paragraphs() {
        let outcome = reflow_snippet("<p>Hello</p><p>world.</p>");

        assert_eq!(outcome.merges.len(), 1);
        assert!(outcome.xhtml.contains("<p>Hello world.</p>"));
        assert_eq!(outcome.paragraphs_before, 2);
        assert_eq!(outcome.paragraphs_after, 1);
    }

    #[test]
    fn non_paragraph_blocks_do_not_merge() {
        assert_eq!(merge_count("<h1>Hello</h1><p>world.</p>"), 0);
    }

    #[test]
    fn terminal_punctuation_blocks_merge() {
        assert_eq!(merge_count("<p>Hello.</p><p>world.</p>"), 0);
    }

    #[test]
    fn terminal_punctuation_before_closing_quote_blocks_merge() {
        assert_eq!(merge_count("<p>Hello.”</p><p>world.</p>"), 0);
    }

    #[test]
    fn non_lowercase_second_paragraph_blocks_merge() {
        assert_eq!(merge_count("<p>Hello</p><p>World.</p>"), 0);
    }

    #[test]
    fn unicode_lowercase_starts_merge() {
        let outcome = reflow_snippet("<p>Hello</p><p>éclat.</p>");

        assert_eq!(outcome.merges.len(), 1);
        assert!(outcome.xhtml.contains("<p>Hello éclat.</p>"));
    }

    #[test]
    fn unequal_classes_block_merge() {
        assert_eq!(
            merge_count(r#"<p class="body">Hello</p><p class="note">world.</p>"#),
            0
        );
    }

    #[test]
    fn second_paragraph_id_blocks_merge() {
        assert_eq!(merge_count(r#"<p>Hello</p><p id="anchor">world.</p>"#), 0);
    }

    #[test]
    fn empty_paragraph_blocks_merge() {
        assert_eq!(merge_count("<p>Hello</p><p> </p>"), 0);
    }

    #[test]
    fn nested_block_blocks_merge() {
        assert_eq!(
            merge_count("<p>Hello</p><p><span>world</span><div>block</div></p>"),
            0
        );
    }

    #[test]
    fn image_blocks_merge() {
        assert_eq!(
            merge_count(r#"<p>Hello</p><p>world <img src="cover.jpg"/></p>"#),
            0
        );
    }

    #[test]
    fn non_whitespace_between_paragraphs_blocks_merge() {
        assert_eq!(merge_count("<p>Hello</p><!-- page --><p>world.</p>"), 0);
    }

    #[test]
    fn whitespace_between_paragraphs_still_merges() {
        assert_eq!(merge_count("<p>Hello</p>\n  <p>world.</p>"), 1);
    }

    #[test]
    fn chains_successive_merges() {
        let outcome = reflow_snippet("<p>Alpha</p><p>beta</p><p>gamma.</p>");

        assert_eq!(outcome.merges.len(), 2);
        assert!(outcome.xhtml.contains("<p>Alpha beta gamma.</p>"));
        assert_eq!(outcome.paragraphs_after, 1);
    }

    #[test]
    fn dehyphenates_word_boundary() {
        let outcome = reflow_snippet("<p>trans-</p><p>lation.</p>");

        assert_eq!(outcome.merges.len(), 1);
        assert!(outcome.xhtml.contains("<p>translation.</p>"));
        assert!(outcome.merges[0].dehyphenated);
    }

    #[test]
    fn dehyphenation_trims_right_leading_whitespace() {
        let outcome = reflow_snippet("<p>trans-</p><p>\n  lation.</p>");

        assert_eq!(outcome.merges.len(), 1);
        assert!(
            outcome.xhtml.contains("<p>translation.</p>"),
            "got: {}",
            outcome.xhtml
        );
    }

    #[test]
    fn appends_inline_children_verbatim() {
        let outcome = reflow_snippet(r#"<p>Hello <em>dear</em></p><p><strong>world</strong>.</p>"#);

        assert_eq!(outcome.merges.len(), 1);
        assert!(
            outcome
                .xhtml
                .contains("<p>Hello <em>dear</em> <strong>world</strong>.</p>")
        );
    }

    #[test]
    fn report_records_merge_context() {
        let outcome = reflow_snippet(
            "<p>Alpha line carries enough text for preview</p><p>beta line continues.</p>",
        );

        assert_eq!(
            outcome.merges[0],
            ReflowMergeRecord {
                resource: "chapter.xhtml".to_string(),
                block_index: 0,
                merged_block_index: 1,
                left_preview: "Alpha line carries enough text for previ...".to_string(),
                right_preview: "beta line continues.".to_string(),
                dehyphenated: false,
            }
        );
    }

    #[test]
    fn dry_run_writes_no_epub() {
        let input = create_minimal_epub("<p>Hello</p><p>world.</p>");
        let output = unique_temp_path("bookforge-reflow-dry-run", "epub");
        let _ = fs::remove_file(&output);

        let outcome = reflow_epub(&input, &output, &ReflowOptions { dry_run: true })
            .expect("dry run should reflow");

        assert!(!outcome.output_written);
        assert!(!output.exists());
        assert_eq!(outcome.report.totals.merge_count, 1);

        let _ = fs::remove_file(input);
    }

    #[test]
    fn real_run_writes_reflowed_epub() {
        let input = create_minimal_epub("<p>Hello</p><p>world.</p>");
        let output = unique_temp_path("bookforge-reflow-real-run", "epub");
        let _ = fs::remove_file(&output);

        let outcome = reflow_epub(&input, &output, &ReflowOptions::default())
            .expect("real run should write output");

        assert!(outcome.output_written);
        assert!(output.exists());
        assert_eq!(outcome.report.totals.merge_count, 1);

        let mut archive = ZipArchive::new(File::open(&output).expect("output should open"))
            .expect("output should be a zip");
        let mut chapter = String::new();
        archive
            .by_name("OEBPS/chapter.xhtml")
            .expect("chapter should exist")
            .read_to_string(&mut chapter)
            .expect("chapter should read");
        assert!(chapter.contains("<p>Hello world.</p>"));

        let _ = fs::remove_file(input);
        let _ = fs::remove_file(output);
    }

    fn create_minimal_epub(body: &str) -> PathBuf {
        let path = unique_temp_path("bookforge-reflow-fixture", "epub");
        let _ = fs::remove_file(&path);
        let file = File::create(&path).expect("fixture should create");
        let mut writer = ZipWriter::new(file);
        let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

        writer
            .start_file("mimetype", stored)
            .expect("mimetype should start");
        writer
            .write_all(b"application/epub+zip")
            .expect("mimetype should write");
        writer
            .start_file("META-INF/container.xml", deflated)
            .expect("container should start");
        writer
            .write_all(
                br#"<?xml version="1.0" encoding="utf-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/package.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#,
            )
            .expect("container should write");
        writer
            .start_file("OEBPS/package.opf", deflated)
            .expect("opf should start");
        writer
            .write_all(
                br#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="bookid">fixture</dc:identifier>
    <dc:title>Fixture</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="chapter" href="chapter.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="chapter"/>
  </spine>
</package>"#,
            )
            .expect("opf should write");
        writer
            .start_file("OEBPS/chapter.xhtml", deflated)
            .expect("chapter should start");
        writer
            .write_all(
                format!(
                    r#"<?xml version="1.0" encoding="utf-8"?><html xmlns="http://www.w3.org/1999/xhtml"><body>{body}</body></html>"#
                )
                .as_bytes(),
            )
            .expect("chapter should write");
        writer.finish().expect("fixture should finish");
        path
    }

    fn unique_temp_path(label: &str, extension: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "{label}-{}-{nonce}.{extension}",
            std::process::id()
        ))
    }
}
