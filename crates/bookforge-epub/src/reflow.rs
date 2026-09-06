use crate::util::is_block_level_name;
use std::{
    fs::{self, File},
    io::Write,
    path::Path,
};

use bookforge_core::{BookforgeError, Result};
use quick_xml::{
    Reader, Writer,
    events::{BytesCData, BytesEnd, BytesStart, BytesText, Event},
};
use serde::Serialize;
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

use crate::{
    archive_limits::{
        ArchiveReadBudget, DEFAULT_ARCHIVE_LIMITS, preflight_archive_path,
        validate_archive_metadata,
    },
    util::{
        attr_value_unescaped, commit_staged_output, create_sibling_work_file,
        deterministic_zip_time, ensure_distinct_paths, is_xhtml_resource_name, local_name,
        normalize_space, resolve_general_ref, validate_xml,
    },
};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReflowOptions {
    pub dry_run: bool,
    pub aggressive: bool,
    /// Remove conservative pdftohtml page anchors, running headers,
    /// whitespace furniture, and bold folios before paragraph merging.
    pub pdf_cleanup: bool,
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
    pub removed_furniture: usize,
    /// XHTML resources positively identified as pdftohtml output while
    /// `pdf_cleanup` was enabled.
    pub pdf_documents_detected: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReflowMergeRecord {
    pub resource: String,
    pub block_index: usize,
    pub merged_block_index: usize,
    pub left_preview: String,
    pub right_preview: String,
    pub dehyphenated: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub aggressive: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub left_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub right_class: Option<String>,
}

fn reflow_epub_staged(
    input: &Path,
    output: &Path,
    options: &ReflowOptions,
) -> Result<ReflowReport> {
    let (staged, staged_file) = create_sibling_work_file(output, "reflow")?;
    let result = with_output_writer(input, output, options, staged_file);
    match result {
        Ok(report) => {
            if let Err(error) = commit_staged_output("reflowed", &staged, output) {
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
}

pub fn reflow_epub(input: &Path, output: &Path, options: &ReflowOptions) -> Result<ReflowOutcome> {
    if !options.dry_run {
        ensure_distinct_paths("EPUB input/output", input, output)?;
    }
    let result = if options.dry_run {
        write_reflowed_epub(input, output, options, None)
    } else {
        reflow_epub_staged(input, output, options)
    }?;

    Ok(ReflowOutcome {
        report: result,
        output_written: !options.dry_run,
    })
}

fn with_output_writer(
    input: &Path,
    output: &Path,
    options: &ReflowOptions,
    output_file: File,
) -> Result<ReflowReport> {
    let writer = ZipWriter::new(output_file);
    write_reflowed_epub(input, output, options, Some(writer))
}

fn write_reflowed_epub(
    input: &Path,
    output: &Path,
    options: &ReflowOptions,
    writer: Option<ZipWriter<File>>,
) -> Result<ReflowReport> {
    let source = File::open(input)?;
    preflight_archive_path(input)?;
    let mut archive = ZipArchive::new(source)?;
    // Central-directory validation plus a per-entry read budget bound every
    // decompression below, so `bookforge reflow` inherits the same
    // lying-small-entry defense as the reader and writer paths.
    let mut read_budget = validate_archive_metadata(&mut archive, DEFAULT_ARCHIVE_LIMITS)?;
    let mut report = ReflowReport {
        schema_version: 1,
        input_path: input.display().to_string(),
        output_path: output.display().to_string(),
        totals: ReflowTotals::default(),
        merges: Vec::new(),
    };

    match writer {
        Some(mut writer) => {
            write_mimetype_first(&mut archive, &mut writer, &mut read_budget)?;
            write_archive_entries(
                &mut archive,
                Some(&mut writer),
                &mut read_budget,
                &mut report,
                options,
            )?;
            writer.finish()?;
        }
        None => {
            validate_mimetype(&mut archive, &mut read_budget)?;
            write_archive_entries(&mut archive, None, &mut read_budget, &mut report, options)?;
        }
    }

    Ok(report)
}

fn write_archive_entries(
    archive: &mut ZipArchive<File>,
    mut writer: Option<&mut ZipWriter<File>>,
    read_budget: &mut ArchiveReadBudget,
    report: &mut ReflowReport,
    options: &ReflowOptions,
) -> Result<()> {
    for index in 0..archive.len() {
        let mut file = archive.by_index(index)?;
        let name = file.name().to_string();

        if name == "mimetype" {
            continue;
        }

        if file.is_dir() {
            if let Some(writer) = writer.as_deref_mut() {
                writer
                    .add_directory(name, normalized_entry_options(CompressionMethod::Deflated))?;
            }
            continue;
        }

        // Entries the archive stored uncompressed are copied through with
        // their method preserved (no inflate/deflate round trip); anything
        // else is recompressed with deflate exactly as before. Bytes still
        // pass through the budget either way.
        let compression_method = match file.compression() {
            CompressionMethod::Stored => CompressionMethod::Stored,
            _ => CompressionMethod::Deflated,
        };
        let compressed_size = file.compressed_size();
        let bytes = read_budget.read_entry(&mut file, &name, compressed_size)?;
        let output_bytes = if is_xhtml_resource_name(&name) {
            let xhtml = String::from_utf8(bytes).map_err(|err| {
                BookforgeError::InvalidInput(format!("XHTML resource '{name}' is not UTF-8: {err}"))
            })?;
            let outcome = reflow_xhtml_resource(&xhtml, &name, options)?;
            report.totals.files_checked += 1;
            report.totals.paragraphs_before += outcome.paragraphs_before;
            report.totals.paragraphs_after += outcome.paragraphs_after;
            report.totals.merge_count += outcome.merges.len();
            report.totals.removed_furniture += outcome.removed_furniture;
            report.totals.pdf_documents_detected += usize::from(outcome.pdf_document_detected);
            if outcome.changed {
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
            writer.start_file(name, normalized_entry_options(compression_method))?;
            writer.write_all(&output_bytes)?;
        }
    }

    Ok(())
}

fn normalized_entry_options(compression_method: CompressionMethod) -> SimpleFileOptions {
    SimpleFileOptions::default()
        .compression_method(compression_method)
        .last_modified_time(deterministic_zip_time())
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug)]
struct ResourceReflow {
    xhtml: String,
    paragraphs_before: usize,
    paragraphs_after: usize,
    merges: Vec<ReflowMergeRecord>,
    removed_furniture: usize,
    pdf_document_detected: bool,
    changed: bool,
}

fn reflow_xhtml_resource(
    xhtml: &str,
    resource: &str,
    options: &ReflowOptions,
) -> Result<ResourceReflow> {
    let (mut nodes, paragraphs_before) = parse_xml(xhtml)?;
    let pdf_document_detected = options.pdf_cleanup && is_pdftohtml_document(xhtml);
    let removed_furniture = if pdf_document_detected {
        cleanup_pdf_nodes(&mut nodes)?
    } else {
        0
    };
    let mut merges = Vec::new();
    reflow_nodes(&mut nodes, resource, options, &mut merges)?;
    let paragraphs_after = count_paragraphs(&nodes);

    if merges.is_empty() && removed_furniture == 0 {
        return Ok(ResourceReflow {
            xhtml: xhtml.to_string(),
            paragraphs_before,
            paragraphs_after,
            merges,
            removed_furniture,
            pdf_document_detected,
            changed: false,
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
        removed_furniture,
        pdf_document_detected,
        changed: true,
    })
}

fn is_pdftohtml_document(xhtml: &str) -> bool {
    let lower = xhtml.to_ascii_lowercase();
    lower.contains("name=\"generator\" content=\"pdftohtml")
        || lower.contains("name='generator' content='pdftohtml")
}

fn cleanup_pdf_nodes(nodes: &mut Vec<XmlNode>) -> Result<usize> {
    let mut removed = 0usize;
    for node in nodes.iter_mut() {
        if let XmlNode::Element(element) = node {
            removed += cleanup_pdf_nodes(&mut element.children)?;
            if local_name(element.start.name().as_ref()) == b"p" {
                removed += remove_trailing_pdf_folio(&mut element.children)?;
            }
        }
    }

    let mut index = 0usize;
    while index < nodes.len() {
        if is_numeric_heading(&nodes[index])? || is_pdf_whitespace_paragraph(&nodes[index])? {
            nodes.remove(index);
            removed += 1;
            continue;
        }

        if is_page_anchor_paragraph(&nodes[index])? {
            let anchor_text = paragraph_text(&nodes[index])?.unwrap_or_default();
            let anchor_only_or_header =
                anchor_text.trim().is_empty() || is_short_running_header(anchor_text.trim());
            if anchor_only_or_header {
                let anchor_only = anchor_text.trim().is_empty();
                nodes.remove(index);
                removed += 1;

                // When pdftohtml emits the page anchor in an empty p, the
                // running header is the next visible short paragraph.
                if anchor_only {
                    while index < nodes.len() && is_whitespace_node(&nodes[index])? {
                        nodes.remove(index);
                    }
                    if index < nodes.len()
                        && paragraph_text(&nodes[index])?
                            .as_deref()
                            .is_some_and(is_short_running_header)
                    {
                        nodes.remove(index);
                        removed += 1;
                    }
                }
                continue;
            }
        }
        index += 1;
    }
    Ok(removed)
}

fn paragraph_text(node: &XmlNode) -> Result<Option<String>> {
    match node {
        XmlNode::Element(element) if local_name(element.start.name().as_ref()) == b"p" => {
            Ok(Some(visible_text(&element.children)?))
        }
        _ => Ok(None),
    }
}

fn is_short_running_header(text: &str) -> bool {
    let text = text.trim();
    !text.is_empty()
        && text.chars().count() <= 80
        && text.split_whitespace().count() <= 8
        && !ends_with_terminal_punctuation(text)
        && !text.to_ascii_uppercase().contains("CAPITOLO")
}

fn is_numeric_heading(node: &XmlNode) -> Result<bool> {
    let XmlNode::Element(element) = node else {
        return Ok(false);
    };
    if !matches!(
        local_name(element.start.name().as_ref()),
        b"h1" | b"h2" | b"h3"
    ) {
        return Ok(false);
    }
    let text = visible_text(&element.children)?;
    // pdftohtml converts folio page numbers into bare headings, but real
    // books also carry numeric headings ("1984"). Only short digit runs —
    // at most three digits, the folio range — are treated as removable
    // furniture; longer numbers survive cleanup.
    let digits = text.chars().filter(|ch| ch.is_ascii_digit()).count();
    Ok(!text.trim().is_empty()
        && digits <= 3
        && text
            .chars()
            .all(|ch| ch.is_ascii_digit() || ch.is_whitespace()))
}

fn is_pdf_whitespace_paragraph(node: &XmlNode) -> Result<bool> {
    let XmlNode::Element(element) = node else {
        return Ok(false);
    };
    if local_name(element.start.name().as_ref()) != b"p" {
        return Ok(false);
    }
    let class = attr_value_unescaped(&element.start, b"class")?.unwrap_or_default();
    Ok(matches!(class.as_str(), "whitespace1" | "softbreak")
        && visible_text(&element.children)?.trim().is_empty())
}

fn is_page_anchor_paragraph(node: &XmlNode) -> Result<bool> {
    let XmlNode::Element(element) = node else {
        return Ok(false);
    };
    Ok(local_name(element.start.name().as_ref()) == b"p"
        && contains_page_anchor(&element.children)?)
}

fn contains_page_anchor(nodes: &[XmlNode]) -> Result<bool> {
    for node in nodes {
        let start = match node {
            XmlNode::Element(element) => Some(&element.start),
            XmlNode::Empty { start } => Some(start),
            XmlNode::Leaf(_) => None,
        };
        if let Some(start) = start
            && local_name(start.name().as_ref()) == b"a"
            && attr_value_unescaped(start, b"id")?
                .as_deref()
                .is_some_and(is_pdf_page_anchor_id)
        {
            return Ok(true);
        }
        if let XmlNode::Element(element) = node
            && contains_page_anchor(&element.children)?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn is_pdf_page_anchor_id(id: &str) -> bool {
    id.strip_prefix('p')
        .is_some_and(|digits| !digits.is_empty() && digits.chars().all(|ch| ch.is_ascii_digit()))
}

fn remove_trailing_pdf_folio(nodes: &mut Vec<XmlNode>) -> Result<usize> {
    let Some(index) = nodes
        .iter()
        .rposition(|node| !is_whitespace_node(node).unwrap_or(false))
    else {
        return Ok(0);
    };
    let XmlNode::Element(element) = &nodes[index] else {
        return Ok(0);
    };
    if local_name(element.start.name().as_ref()) != b"b"
        || attr_value_unescaped(&element.start, b"class")?.as_deref() != Some("calibre7")
    {
        return Ok(0);
    }
    let text = visible_text(&element.children)?;
    if text.trim().is_empty()
        || !text
            .chars()
            .all(|ch| ch.is_ascii_digit() || ch.is_whitespace())
    {
        return Ok(0);
    }
    nodes.remove(index);
    Ok(1)
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

/// Audit EPUB P2 (crash vector): the reflow walkers (`reflow_nodes`,
/// `cleanup_pdf_nodes`, `contains_page_anchor`, `count_paragraphs*`,
/// `visible_text`/`append_visible_text`, `write_nodes`) recurse once per
/// tree level, so a hostile ~10^5-deep element ladder stack-aborts
/// `bookforge reflow` before any result can be produced. `parse_xml` is
/// already iterative; capping nesting depth HERE — at parse time, where the
/// event stream is naturally flat — protects every downstream walker in one
/// place. Genuine EPUB XHTML nests a few dozen levels deep at worst, so
/// 10_000 leaves a >100x margin for legitimately deep documents while
/// turning hostile ladders into the standard graceful `InvalidInput`
/// rejection instead of a process abort.
const MAX_XHTML_NESTING_DEPTH: usize = 10_000;

fn parse_xml(xml: &str) -> Result<(Vec<XmlNode>, usize)> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut roots = Vec::new();
    let mut stack = Vec::<BuildingElement>::new();
    let mut paragraph_count = 0usize;

    loop {
        match reader.read_event()? {
            Event::Start(element) => {
                if stack.len() >= MAX_XHTML_NESTING_DEPTH {
                    return Err(BookforgeError::InvalidInput(format!(
                        "XHTML nesting exceeds the supported depth of {MAX_XHTML_NESTING_DEPTH}"
                    )));
                }
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
    options: &ReflowOptions,
    merges: &mut Vec<ReflowMergeRecord>,
) -> Result<()> {
    for node in nodes.iter_mut() {
        if let XmlNode::Element(element) = node {
            reflow_nodes(&mut element.children, resource, options, merges)?;
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
            (XmlNode::Element(left), XmlNode::Element(right)) => {
                merge_decision(left, right, options)?
            }
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
    aggressive: bool,
    left_class: Option<String>,
    right_class: Option<String>,
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
            aggressive: self.aggressive,
            left_class: self.left_class,
            right_class: self.right_class,
        }
    }
}

fn merge_decision(
    left: &XmlElement,
    right: &XmlElement,
    options: &ReflowOptions,
) -> Result<Option<MergeDecision>> {
    let left_text = visible_text(&left.children)?;
    let right_text = visible_text(&right.children)?;

    if left_text.trim().is_empty() || right_text.trim().is_empty() {
        return Ok(None);
    }
    if !left_text.chars().any(char::is_alphabetic) {
        return Ok(None);
    }
    if ends_with_terminal_punctuation(&left_text) {
        return Ok(None);
    }
    let Some(right_start) = right_start_mode(&right_text, options.aggressive) else {
        return Ok(None);
    };
    let left_class = attr_value_unescaped(&left.start, b"class")?;
    let right_class = attr_value_unescaped(&right.start, b"class")?;
    let class_mismatch = left_class != right_class;
    if class_mismatch && !options.aggressive {
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
        aggressive: right_start == RightStartMode::Aggressive || class_mismatch,
        left_class: if class_mismatch { left_class } else { None },
        right_class: if class_mismatch { right_class } else { None },
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
            text.push_str(&resolve_general_ref(reference)?);
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
            .chars()
            .all(char::is_whitespace)),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RightStartMode {
    Conservative,
    Aggressive,
}

fn right_start_mode(text: &str, aggressive: bool) -> Option<RightStartMode> {
    let first = text.trim_start().chars().next()?;
    if first.is_lowercase() {
        return Some(RightStartMode::Conservative);
    }
    if !aggressive || !text.chars().any(char::is_alphabetic) || !is_aggressive_right_start(first) {
        return None;
    }
    Some(RightStartMode::Aggressive)
}

fn is_aggressive_right_start(ch: char) -> bool {
    ch.is_uppercase() || matches!(ch, '“' | '‘' | '"' | '\'' | '«' | '(' | '[' | '—' | '–')
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
                    .chars()
                    .all(char::is_whitespace)
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

fn validate_mimetype(
    archive: &mut ZipArchive<File>,
    read_budget: &mut ArchiveReadBudget,
) -> Result<()> {
    let mimetype = crate::archive_limits::read_archive_text(archive, read_budget, "mimetype")?;
    if mimetype.trim() != "application/epub+zip" {
        return Err(BookforgeError::InvalidInput(
            "EPUB mimetype must be application/epub+zip".to_string(),
        ));
    }
    Ok(())
}

fn write_mimetype_first(
    source: &mut ZipArchive<File>,
    writer: &mut ZipWriter<File>,
    read_budget: &mut ArchiveReadBudget,
) -> Result<()> {
    let mimetype = crate::archive_limits::read_archive_text(source, read_budget, "mimetype")?;
    if mimetype.trim() != "application/epub+zip" {
        return Err(BookforgeError::InvalidInput(
            "EPUB mimetype must be application/epub+zip".to_string(),
        ));
    }

    let stored = normalized_entry_options(CompressionMethod::Stored);
    writer.start_file("mimetype", stored)?;
    writer.write_all(b"application/epub+zip")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs::File,
        io::{Read, Write},
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };
    use zip::{DateTime, ZipWriter, write::SimpleFileOptions};

    fn reflow_snippet_with_options(body: &str, options: &ReflowOptions) -> ResourceReflow {
        let xhtml = format!("<html><body>{body}</body></html>");
        reflow_xhtml_resource(&xhtml, "chapter.xhtml", options).expect("snippet should reflow")
    }

    fn reflow_snippet(body: &str) -> ResourceReflow {
        reflow_snippet_with_options(body, &ReflowOptions::default())
    }

    fn merge_count(body: &str) -> usize {
        reflow_snippet(body).merges.len()
    }

    fn aggressive_options() -> ReflowOptions {
        ReflowOptions {
            aggressive: true,
            ..ReflowOptions::default()
        }
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
    fn pdf_cleanup_removes_furniture_then_merges_page_spanning_paragraph() {
        let xhtml = r#"<html><head><meta name="generator" content="pdftohtml 0.36"/></head><body>
            <p class="calibre1">La frase continua <b class="calibre7">27</b></p>
            <p class="whitespace1">&#160;</p>
            <p class="calibre1"><a id="p28"></a></p>
            <p class="calibre1">Il Mondo al Contrario</p>
            <p class="calibre1">nella pagina seguente.</p>
        </body></html>"#;
        let outcome = reflow_xhtml_resource(
            xhtml,
            "chapter.xhtml",
            &ReflowOptions {
                aggressive: true,
                pdf_cleanup: true,
                ..ReflowOptions::default()
            },
        )
        .expect("PDF-derived fixture should clean and reflow");

        assert_eq!(outcome.removed_furniture, 4);
        assert_eq!(outcome.merges.len(), 1);
        assert!(
            outcome
                .xhtml
                .contains("La frase continua nella pagina seguente.")
        );
        assert!(!outcome.xhtml.contains("Il Mondo al Contrario"));
        assert!(!outcome.xhtml.contains(">27<"));
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
    fn aggressive_merges_uppercase_quote_bracket_and_dash_starts() {
        let cases = [
            "<p>review of David</p><p>Toop's Rap Attack.</p>",
            "<p>Hello</p><p>“World.”</p>",
            "<p>Hello</p><p>[World].</p>",
            "<p>Hello</p><p>— World.</p>",
        ];
        let options = aggressive_options();

        for body in cases {
            assert_eq!(merge_count(body), 0, "default should not merge {body}");
            let outcome = reflow_snippet_with_options(body, &options);
            assert_eq!(outcome.merges.len(), 1, "aggressive should merge {body}");
            assert!(outcome.merges[0].aggressive);
        }
    }

    #[test]
    fn aggressive_rejects_letterless_right_paragraph() {
        let options = aggressive_options();

        assert_eq!(
            reflow_snippet_with_options("<p>Hello</p><p>— 12 —</p>", &options)
                .merges
                .len(),
            0
        );
        assert_eq!(
            reflow_snippet_with_options("<p>Hello</p><p>[123]</p>", &options)
                .merges
                .len(),
            0
        );
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
    fn aggressive_merges_unequal_classes_and_records_audit_fields() {
        let outcome = reflow_snippet_with_options(
            r#"<p class="calibre6">review of David</p><p class="calibre1">Toop’s Rap Attack.</p>"#,
            &aggressive_options(),
        );

        assert_eq!(outcome.merges.len(), 1);
        assert!(
            outcome
                .xhtml
                .contains(r#"<p class="calibre6">review of David Toop’s Rap Attack.</p>"#),
            "got: {}",
            outcome.xhtml
        );
        assert!(outcome.merges[0].aggressive);
        assert_eq!(outcome.merges[0].left_class.as_deref(), Some("calibre6"));
        assert_eq!(outcome.merges[0].right_class.as_deref(), Some("calibre1"));
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
    fn letterless_left_paragraph_blocks_merge() {
        // Bare page/footnote numbers from PDF conversions must not be
        // glued into prose (§9c.2 condition 7).
        assert_eq!(merge_count("<p>1</p><p>doubt what attracted.</p>"), 0);
        assert_eq!(merge_count("<p>— 12 —</p><p>opening up.</p>"), 0);
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
                aggressive: false,
                left_class: None,
                right_class: None,
            }
        );
    }

    #[test]
    fn report_marks_only_relaxed_rule_merges_as_aggressive() {
        let outcome = reflow_snippet_with_options(
            r#"<p class="body">Hello</p><p class="body">world</p><p class="note">again.</p>"#,
            &aggressive_options(),
        );

        assert_eq!(outcome.merges.len(), 2);
        assert!(!outcome.merges[0].aggressive);
        assert!(outcome.merges[1].aggressive);
        assert_eq!(outcome.merges[1].left_class.as_deref(), Some("body"));
        assert_eq!(outcome.merges[1].right_class.as_deref(), Some("note"));
        assert!(
            serde_json::to_string_pretty(&outcome.merges[1])
                .expect("record should serialize")
                .contains(r#""aggressive": true"#)
        );
        assert!(
            serde_json::to_string_pretty(&outcome.merges[1])
                .expect("record should serialize")
                .contains(r#""left_class": "body""#)
        );
        assert!(
            serde_json::to_string_pretty(&outcome.merges[1])
                .expect("record should serialize")
                .contains(r#""right_class": "note""#)
        );
    }

    #[test]
    fn conservative_record_json_omits_aggressive_field() {
        let outcome = reflow_snippet("<p>Hello</p><p>world.</p>");
        let json =
            serde_json::to_string_pretty(&outcome.merges[0]).expect("record should serialize");

        assert_eq!(
            json,
            r#"{
  "resource": "chapter.xhtml",
  "block_index": 0,
  "merged_block_index": 1,
  "left_preview": "Hello",
  "right_preview": "world.",
  "dehyphenated": false
}"#
        );
    }

    #[test]
    fn dry_run_writes_no_epub() {
        let input = create_minimal_epub("<p>Hello</p><p>world.</p>");
        let output = unique_temp_path("bookforge-reflow-dry-run", "epub");
        let _ = fs::remove_file(&output);

        let outcome = reflow_epub(
            &input,
            &output,
            &ReflowOptions {
                dry_run: true,
                ..ReflowOptions::default()
            },
        )
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

    #[test]
    fn reflow_rejects_archives_over_the_entry_count_limit() {
        let path = unique_temp_path("bookforge-reflow-entry-bomb", "epub");
        let _ = fs::remove_file(&path);
        let file = File::create(&path).expect("fixture should create");
        let mut writer = ZipWriter::new(file);
        let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        writer
            .start_file("mimetype", stored)
            .expect("mimetype should start");
        writer
            .write_all(b"application/epub+zip")
            .expect("mimetype should write");
        // 10_001 entries exceed MAX_ARCHIVE_ENTRIES: metadata validation
        // must reject the archive before any entry is decompressed.
        for index in 0..10_001 {
            writer
                .start_file(format!("blob{index}"), stored)
                .expect("blob should start");
            writer.write_all(b"x").expect("blob should write");
        }
        writer.finish().expect("fixture should finish");

        let output = unique_temp_path("bookforge-reflow-entry-bomb-out", "epub");

        let error = reflow_epub(&path, &output, &ReflowOptions::default())
            .expect_err("entry-count bomb must be rejected");

        assert!(error.to_string().contains("entry count limit exceeded"));
        assert!(!output.exists(), "no output may be committed on rejection");

        let _ = fs::remove_file(path);
    }

    #[test]
    fn hostile_deep_element_ladder_is_rejected_gracefully_not_stack_aborted() {
        // Audit EPUB P2: a ~10^5-deep element ladder used to stack-abort the
        // reflow walkers (reflow_nodes, count_paragraphs, visible_text and
        // friends all recurse once per tree level). The parse-time depth cap
        // turns the same document into a clean InvalidInput error attributed
        // to the depth limit — the process returns, it does not abort.
        const LADDER_DEPTH: usize = 60_000;
        let mut ladder = String::new();
        for index in 0..LADDER_DEPTH {
            // A unique attribute per level defeats text compression so the
            // fixture reaches the depth cap without tripping the archive's
            // 100:1 decompression-ratio guard first.
            ladder.push_str(&format!("<div data-i=\"{index}\">"));
        }
        ladder.push_str("<p class=\"body\">bottom</p>");
        for _ in 0..LADDER_DEPTH {
            ladder.push_str("</div>");
        }

        let input = create_minimal_epub(&ladder);
        let output = unique_temp_path("bookforge-reflow-ladder-out", "epub");
        let _ = fs::remove_file(&output);

        let error = reflow_epub(&input, &output, &ReflowOptions::default())
            .expect_err("a hostile element ladder must be rejected, not abort");

        assert!(
            error.to_string().contains("exceeds the supported depth"),
            "the rejection must attribute itself to the depth cap: {error}"
        );
        assert!(!output.exists(), "no output may be committed on rejection");

        let _ = fs::remove_file(input);
    }

    #[test]
    fn stored_entries_are_copied_through_and_timestamps_are_normalized() {
        let input = unique_temp_path("bookforge-reflow-stored", "epub");
        let _ = fs::remove_file(&input);
        let file = File::create(&input).expect("fixture should create");
        let mut writer = ZipWriter::new(file);
        let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        let deflated = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .last_modified_time(deterministic_zip_time());
        writer
            .start_file("mimetype", stored)
            .expect("mimetype should start");
        writer
            .write_all(b"application/epub+zip")
            .expect("mimetype should write");
        writer
            .start_file("OEBPS/chapter.xhtml", deflated)
            .expect("chapter should start");
        writer
            .write_all(b"<html><body><p>Hello</p><p>world.</p></body></html>")
            .expect("chapter should write");
        let image_options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .last_modified_time(
                DateTime::from_date_and_time(2024, 5, 6, 7, 8, 9)
                    .expect("test timestamp should be valid"),
            );
        writer
            .start_file("OEBPS/cover.png", image_options)
            .expect("image should start");
        writer
            .write_all(&[0u8, 1, 2, 3, 250])
            .expect("image should write");
        writer.finish().expect("fixture should finish");

        let output = unique_temp_path("bookforge-reflow-stored-out", "epub");
        reflow_epub(&input, &output, &ReflowOptions::default()).expect("reflow should succeed");

        let mut archive =
            ZipArchive::new(File::open(&output).expect("output should open")).expect("zip");
        let mut copied = Vec::new();
        {
            let mut cover = archive
                .by_name("OEBPS/cover.png")
                .expect("cover should exist");
            assert_eq!(cover.compression(), CompressionMethod::Stored);
            assert_eq!(
                cover.last_modified(),
                Some(deterministic_zip_time()),
                "copied-through entries must not carry mixed source timestamps"
            );
            cover.read_to_end(&mut copied).unwrap();
        }
        assert_eq!(copied, vec![0u8, 1, 2, 3, 250]);

        let _ = fs::remove_file(input);
        let _ = fs::remove_file(output);
    }

    #[test]
    fn pdf_cleanup_preserves_numeric_book_title_headings() {
        let xhtml = r#"<html><head><meta name="generator" content="pdftohtml 0.36"/></head><body>
            <h2>1984</h2>
            <h2>7</h2>
            <p>Prose continues here.</p>
        </body></html>"#;
        let outcome = reflow_xhtml_resource(
            xhtml,
            "chapter.xhtml",
            &ReflowOptions {
                pdf_cleanup: true,
                ..ReflowOptions::default()
            },
        )
        .expect("PDF-derived fixture should clean and reflow");

        assert!(
            outcome.xhtml.contains("<h2>1984</h2>"),
            "multi-digit titles like '1984' are real headings, got: {}",
            outcome.xhtml
        );
        assert!(
            !outcome.xhtml.contains(">7<"),
            "single folio digits remain removable furniture, got: {}",
            outcome.xhtml
        );
        assert_eq!(outcome.removed_furniture, 1);
    }
}
