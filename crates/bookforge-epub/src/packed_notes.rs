//! Split paragraphs that encode a sequence of individually anchored notes.
//! Reader and writer must normalize identically so their DOM paths agree.
use std::borrow::Cow;

use bookforge_core::{BookforgeError, Result};
use quick_xml::{
    Reader, Writer,
    events::{BytesStart, Event},
};

use crate::util::{attr_value_unescaped, local_name, never_translate_element, resolve_general_ref};

struct Child {
    offset: usize,
    anchor: bool,
    id: bool,
    backlink: bool,
    text: String,
}

impl Child {
    fn new(element: &BytesStart<'_>, offset: usize) -> Result<Self> {
        let anchor = local_name(element.name().as_ref()) == b"a";
        let id = anchor && attr_value_unescaped(element, b"id")?.is_some_and(|id| !id.is_empty());
        let backlink = anchor
            && attr_value_unescaped(element, b"href")?.is_some_and(|href| {
                // A note backlink is an internal fragment reference, not an
                // external citation whose label happens to be a number.
                href.split_once('#').is_some_and(|(path, fragment)| {
                    !fragment.is_empty() && !path.contains(':') && !path.starts_with("//")
                })
            });
        Ok(Self {
            offset,
            anchor,
            id,
            backlink,
            text: String::new(),
        })
    }
}

struct Paragraph {
    depth: usize,
    opening: BytesStart<'static>,
    child: Option<Child>,
    children: usize,
    prefix_is_whitespace: bool,
    first_child_offset: Option<usize>,
    pending_anchor: Option<usize>,
    notes: Vec<(usize, usize)>,
}

impl Paragraph {
    fn text(&mut self, text: &str) {
        if let Some(child) = &mut self.child {
            if child.anchor {
                child.text.push_str(text);
            }
        } else if !text.chars().all(char::is_whitespace) {
            if self.children == 0 {
                self.prefix_is_whitespace = false;
            }
            self.pending_anchor = None;
        }
    }

    fn finish_child(&mut self, child: Child) {
        if self.children == 0 {
            self.first_child_offset = Some(child.offset);
        }
        if child.backlink
            && let Some(offset) = self.pending_anchor
        {
            let text = child.text.trim();
            if !text.is_empty()
                && text.bytes().all(|b| b.is_ascii_digit())
                && let Ok(number) = text.parse::<usize>()
            {
                self.notes.push((offset, number));
            }
        }
        let empty_anchor = child.anchor && child.id && child.text.trim().is_empty();
        if self.children == 0 && !empty_anchor {
            self.prefix_is_whitespace = false;
        }
        self.pending_anchor = empty_anchor.then_some(child.offset);
        self.children += 1;
    }

    fn insertions(self) -> Result<Vec<(usize, String)>> {
        if !self.prefix_is_whitespace
            || self.notes.len() < 2
            || self.notes.first().map(|note| note.0) != self.first_child_offset
            || !self
                .notes
                .windows(2)
                .all(|pair| pair[0].1.checked_add(1) == Some(pair[1].1))
        {
            return Ok(Vec::new());
        }
        let mut repeated = self.opening.clone();
        repeated.clear_attributes();
        for attribute in self.opening.attributes() {
            let attribute =
                attribute.map_err(|error| BookforgeError::InvalidInput(error.to_string()))?;
            if !matches!(attribute.key.as_ref(), b"id" | b"xml:id") {
                repeated.push_attribute(attribute);
            }
        }
        let mut writer = Writer::new(Vec::new());
        writer.write_event(Event::End(self.opening.to_end()))?;
        writer.write_event(Event::Start(repeated))?;
        let boundary = String::from_utf8(writer.into_inner())
            .map_err(|error| BookforgeError::InvalidInput(error.to_string()))?;
        Ok(self
            .notes
            .into_iter()
            .skip(1)
            .map(|(offset, _)| (offset, boundary.clone()))
            .collect())
    }
}

/// Recognize an empty ID anchor followed by a numbered internal backlink at
/// the beginning of a paragraph and repeated with consecutive note numbers.
/// Ordinary prose, nonconsecutive citations, and external numeric links stay
/// byte-for-byte unchanged. Only paragraph boundaries are inserted; all source
/// text, anchor IDs, links, and formatting bytes are retained.
pub(crate) fn normalize_packed_notes(xhtml: &str) -> Result<Cow<'_, str>> {
    let mut reader = Reader::from_str(xhtml);
    reader.config_mut().trim_text(false);
    let mut depth = 0usize;
    let mut suppressed_at = None;
    let mut paragraph: Option<Paragraph> = None;
    let mut insertions = Vec::new();
    loop {
        let offset = reader.buffer_position() as usize;
        match reader.read_event()? {
            Event::Start(element) => {
                depth += 1;
                if suppressed_at.is_none()
                    && never_translate_element(local_name(element.name().as_ref()))
                {
                    suppressed_at = Some(depth);
                }
                if let Some(p) = &mut paragraph {
                    if depth == p.depth + 1 {
                        p.child = Some(Child::new(&element, offset)?);
                    }
                } else if suppressed_at.is_none() && local_name(element.name().as_ref()) == b"p" {
                    paragraph = Some(Paragraph {
                        depth,
                        opening: element.into_owned(),
                        child: None,
                        children: 0,
                        prefix_is_whitespace: true,
                        first_child_offset: None,
                        pending_anchor: None,
                        notes: Vec::new(),
                    });
                }
            }
            Event::Empty(element) => {
                if let Some(p) = &mut paragraph
                    && depth == p.depth
                {
                    p.finish_child(Child::new(&element, offset)?);
                }
            }
            Event::End(_) => {
                if paragraph.as_ref().is_some_and(|p| p.depth == depth) {
                    insertions.extend(
                        paragraph
                            .take()
                            .expect("paragraph depth matched")
                            .insertions()?,
                    );
                } else if let Some(p) = &mut paragraph
                    && depth == p.depth + 1
                    && let Some(child) = p.child.take()
                {
                    p.finish_child(child);
                }
                if suppressed_at == Some(depth) {
                    suppressed_at = None;
                }
                depth = depth.saturating_sub(1);
            }
            Event::Text(text) => {
                if let Some(p) = &mut paragraph {
                    p.text(
                        &text
                            .html_content()
                            .map_err(|error| BookforgeError::InvalidInput(error.to_string()))?,
                    );
                }
            }
            Event::CData(text) => {
                if let Some(p) = &mut paragraph {
                    p.text(
                        &text
                            .decode()
                            .map_err(|error| BookforgeError::InvalidInput(error.to_string()))?,
                    );
                }
            }
            Event::GeneralRef(reference) => {
                if let Some(p) = &mut paragraph {
                    p.text(&resolve_general_ref(&reference)?);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if insertions.is_empty() {
        return Ok(Cow::Borrowed(xhtml));
    }
    let mut output = String::with_capacity(xhtml.len());
    let mut copied = 0;
    for (offset, boundary) in insertions {
        output.push_str(&xhtml[copied..offset]);
        output.push_str(&boundary);
        copied = offset;
    }
    output.push_str(&xhtml[copied..]);
    Ok(Cow::Owned(output))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_consecutive_notes_without_changing_markup_or_duplicating_ids() {
        let source = r#"<html><body><p id="notes" class="indent">&#160;<a id="n1"></a><a href="chapter.xhtml#r1">1</a> First <i>note</i>. <a id="n2"/><a href="chapter.xhtml#r2">2</a> Second &amp; final.</p><p>After.</p></body></html>"#;
        let expected = source.replace("<a id=\"n2\"/>", "</p><p class=\"indent\"><a id=\"n2\"/>");
        let normalized = normalize_packed_notes(source).unwrap();
        assert_eq!(normalized, expected);
        assert_eq!(normalize_packed_notes(&normalized).unwrap(), normalized);
    }

    #[test]
    fn preserves_prose_external_links_and_nonconsecutive_references() {
        for source in [
            r##"<p>Prose <a id="n1"/><a href="#r1">1</a> and <a id="n2"/><a href="#r2">2</a>.</p>"##,
            r##"<p><a id="n1"/><a href="#r1">1</a> text <a id="n3"/><a href="#r3">3</a> text</p>"##,
            r#"<p><a id="n1"/><a href="https://example.com/#r1">1</a> text <a id="n2"/><a href="https://example.com/#r2">2</a> text</p>"#,
            r##"<svg><p><a id="n1"/><a href="#r1">1</a> text <a id="n2"/><a href="#r2">2</a> text</p></svg>"##,
        ] {
            assert!(
                matches!(normalize_packed_notes(source).unwrap(), Cow::Borrowed(_)),
                "{source}"
            );
        }
    }

    #[test]
    fn keeps_namespace_attributes_and_removes_repeated_xml_id() {
        let source = r##"<h:p xmlns:h="http://www.w3.org/1999/xhtml" xml:id="notes"><h:a id="n1"/><h:a href="#r1">1</h:a>A<h:a id="n2"/><h:a href="#r2">2</h:a>B</h:p>"##;
        let output = normalize_packed_notes(source).unwrap();
        assert!(output.contains(r#"</h:p><h:p xmlns:h="http://www.w3.org/1999/xhtml">"#));
        assert_eq!(output.matches("xml:id").count(), 1);
    }
}
