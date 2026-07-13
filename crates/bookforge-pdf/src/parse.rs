//! Parser for `pdftohtml -xml` output into the page/fragment IR.
//!
//! The format, per page:
//!
//! ```xml
//! <page number="1" width="918" height="1188" ...>
//!   <fontspec id="0" size="17" family="Times" color="#000000"/>
//!   <text top="246" left="261" width="394" height="18" font="0">Line with <b>bold</b></text>
//!   <image top="320" left="120" width="300" height="180" src="paper-1_1.png"/>
//! </page>
//! ```
//!
//! `<text>` content may nest `<b>`, `<i>` (and occasionally `<a>`);
//! styling is flattened into spans, anchors into plain text.

use std::collections::HashMap;

use quick_xml::{Reader, events::Event};

use crate::{
    PdfError, Result,
    model::{Fragment, ImageRegion, Page, Span},
};

pub fn parse_pdf2xml(xml: &str) -> Result<Vec<Page>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);

    let mut pages = Vec::new();
    let mut current_page: Option<Page> = None;
    let mut current_fragment: Option<Fragment> = None;
    let mut bold_depth = 0usize;
    let mut italic_depth = 0usize;

    loop {
        match reader.read_event()? {
            Event::Start(element) | Event::Empty(element)
                if local(element.name().as_ref()) == b"page" =>
            {
                if let Some(page) = current_page.take() {
                    pages.push(page);
                }
                let mut number = 0u32;
                let mut width = 0i32;
                let mut height = 0i32;
                for attr in element.attributes() {
                    let attr = attr.map_err(|err| PdfError::InvalidInput(err.to_string()))?;
                    let value = attr
                        .decoded_and_normalized_value(
                            quick_xml::XmlVersion::Implicit1_0,
                            reader.decoder(),
                        )
                        .map_err(|err| PdfError::InvalidInput(err.to_string()))?;
                    match local(attr.key.as_ref()) {
                        b"number" => number = value.parse().unwrap_or(0),
                        b"width" => width = parse_coord(&value),
                        b"height" => height = parse_coord(&value),
                        _ => {}
                    }
                }
                current_page = Some(Page {
                    number,
                    width,
                    height,
                    fragments: Vec::new(),
                    images: Vec::new(),
                    font_sizes: HashMap::new(),
                });
            }
            Event::Empty(element) if local(element.name().as_ref()) == b"fontspec" => {
                let Some(page) = current_page.as_mut() else {
                    continue;
                };
                let mut id = None;
                let mut size = None;
                for attr in element.attributes() {
                    let attr = attr.map_err(|err| PdfError::InvalidInput(err.to_string()))?;
                    let value = attr
                        .decoded_and_normalized_value(
                            quick_xml::XmlVersion::Implicit1_0,
                            reader.decoder(),
                        )
                        .map_err(|err| PdfError::InvalidInput(err.to_string()))?;
                    match local(attr.key.as_ref()) {
                        b"id" => id = value.parse::<u32>().ok(),
                        b"size" => size = Some(parse_coord(&value).unsigned_abs()),
                        _ => {}
                    }
                }
                if let (Some(id), Some(size)) = (id, size) {
                    page.font_sizes.insert(id, size);
                }
            }
            Event::Start(element) if local(element.name().as_ref()) == b"text" => {
                let mut fragment = Fragment {
                    top: 0,
                    left: 0,
                    width: 0,
                    height: 0,
                    font: 0,
                    spans: Vec::new(),
                };
                for attr in element.attributes() {
                    let attr = attr.map_err(|err| PdfError::InvalidInput(err.to_string()))?;
                    let value = attr
                        .decoded_and_normalized_value(
                            quick_xml::XmlVersion::Implicit1_0,
                            reader.decoder(),
                        )
                        .map_err(|err| PdfError::InvalidInput(err.to_string()))?;
                    match local(attr.key.as_ref()) {
                        b"top" => fragment.top = parse_coord(&value),
                        b"left" => fragment.left = parse_coord(&value),
                        b"width" => fragment.width = parse_coord(&value),
                        b"height" => fragment.height = parse_coord(&value),
                        b"font" => fragment.font = value.parse().unwrap_or(0),
                        _ => {}
                    }
                }
                current_fragment = Some(fragment);
                bold_depth = 0;
                italic_depth = 0;
            }
            Event::Start(element) | Event::Empty(element)
                if local(element.name().as_ref()) == b"image" =>
            {
                let Some(page) = current_page.as_mut() else {
                    continue;
                };
                let mut region = ImageRegion {
                    top: 0,
                    left: 0,
                    width: 0,
                    height: 0,
                    src: None,
                };
                for attr in element.attributes() {
                    let attr = attr.map_err(|err| PdfError::InvalidInput(err.to_string()))?;
                    let value = attr
                        .decoded_and_normalized_value(
                            quick_xml::XmlVersion::Implicit1_0,
                            reader.decoder(),
                        )
                        .map_err(|err| PdfError::InvalidInput(err.to_string()))?;
                    match local(attr.key.as_ref()) {
                        b"top" => region.top = parse_coord(&value),
                        b"left" => region.left = parse_coord(&value),
                        b"width" => region.width = parse_coord(&value),
                        b"height" => region.height = parse_coord(&value),
                        b"src" => region.src = Some(value.into_owned()),
                        _ => {}
                    }
                }
                page.images.push(region);
            }
            Event::Start(element) if current_fragment.is_some() => {
                match local(element.name().as_ref()) {
                    b"b" => bold_depth += 1,
                    b"i" => italic_depth += 1,
                    _ => {}
                }
            }
            Event::End(element) if current_fragment.is_some() => {
                match local(element.name().as_ref()) {
                    b"text" => {
                        let fragment = current_fragment.take().expect("checked above");
                        if let Some(page) = current_page.as_mut()
                            && fragment.spans.iter().any(|span| !span.text.is_empty())
                        {
                            page.fragments.push(fragment);
                        }
                    }
                    b"b" => bold_depth = bold_depth.saturating_sub(1),
                    b"i" => italic_depth = italic_depth.saturating_sub(1),
                    _ => {}
                }
            }
            Event::Text(text) => {
                if let Some(fragment) = current_fragment.as_mut() {
                    let value = text
                        .html_content()
                        .map_err(|err| PdfError::InvalidInput(err.to_string()))?;
                    push_span(fragment, &value, bold_depth > 0, italic_depth > 0);
                }
            }
            Event::GeneralRef(reference) => {
                if let Some(fragment) = current_fragment.as_mut() {
                    if let Some(ch) = reference
                        .resolve_char_ref()
                        .map_err(|err| PdfError::InvalidInput(err.to_string()))?
                    {
                        push_span(fragment, &ch.to_string(), bold_depth > 0, italic_depth > 0);
                    } else {
                        let name = reference
                            .decode()
                            .map_err(|err| PdfError::InvalidInput(err.to_string()))?;
                        if let Some(value) = quick_xml::escape::resolve_html5_entity(&name) {
                            push_span(fragment, value, bold_depth > 0, italic_depth > 0);
                        }
                    }
                }
            }
            Event::End(element) if local(element.name().as_ref()) == b"page" => {
                if let Some(page) = current_page.take() {
                    pages.push(page);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    if let Some(page) = current_page.take() {
        pages.push(page);
    }

    Ok(pages)
}

fn push_span(fragment: &mut Fragment, text: &str, bold: bool, italic: bool) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = fragment.spans.last_mut()
        && last.bold == bold
        && last.italic == italic
    {
        last.text.push_str(text);
        return;
    }
    fragment.spans.push(Span {
        text: text.to_string(),
        bold,
        italic,
    });
}

/// pdftohtml emits integer coordinates, but some builds produce
/// fractional values; accept both.
fn parse_coord(value: &str) -> i32 {
    value
        .parse::<i32>()
        .or_else(|_| value.parse::<f64>().map(|f| f.round() as i32))
        .unwrap_or(0)
}

fn local(name: &[u8]) -> &[u8] {
    name.rsplit(|byte| *byte == b':').next().unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<pdf2xml producer="poppler" version="24.02.0">
<page number="1" position="absolute" top="0" left="0" height="1188" width="918">
  <fontspec id="0" size="22" family="Times" color="#000000"/>
  <fontspec id="1" size="11" family="Times" color="#000000"/>
  <text top="100" left="200" width="500" height="24" font="0">A <b>Bold</b> Title</text>
  <image top="150" left="210" width="120" height="80" src="fig-1.png"/>
  <text top="200" left="100" width="350" height="12" font="1">Left column line with <i>italics</i>.</text>
  <text top="200" left="480" width="350" height="12" font="1">Right column &amp; more.</text>
</page>
</pdf2xml>"##;

    #[test]
    fn parses_pages_fonts_and_styled_fragments() {
        let pages = parse_pdf2xml(SAMPLE).expect("sample should parse");
        assert_eq!(pages.len(), 1);
        let page = &pages[0];
        assert_eq!(page.number, 1);
        assert_eq!(page.width, 918);
        assert_eq!(page.font_sizes.get(&0), Some(&22));
        assert_eq!(page.fragments.len(), 3);
        assert_eq!(page.images.len(), 1);
        assert_eq!(page.images[0].top, 150);
        assert_eq!(page.images[0].src.as_deref(), Some("fig-1.png"));

        let title = &page.fragments[0];
        assert_eq!(title.font, 0);
        assert_eq!(
            title.spans,
            vec![
                Span {
                    text: "A ".into(),
                    bold: false,
                    italic: false
                },
                Span {
                    text: "Bold".into(),
                    bold: true,
                    italic: false
                },
                Span {
                    text: " Title".into(),
                    bold: false,
                    italic: false
                },
            ]
        );

        let right = &page.fragments[2];
        assert_eq!(right.spans[0].text, "Right column & more.");
    }
}
