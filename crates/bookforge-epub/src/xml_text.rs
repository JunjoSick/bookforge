//! Visible-text chunking shared by the reader and the writer.
//!
//! quick-xml reports one run of visible text as several events: plain text,
//! general entity references (`&amp;`, `&#8217;`) and CDATA sections arrive
//! separately. The reader turns loose text into addressable text-node blocks
//! and the writer finds them again by counting non-whitespace text nodes per
//! parent element, so both sides must agree on where one text node ends.
//! Adjacent text, reference and CDATA events form one chunk; any other event
//! (an element, a comment, a processing instruction) ends it.

use bookforge_core::Result;
use quick_xml::{Reader, events::Event};

use crate::util::resolve_general_ref;

pub(crate) struct TextChunk<'a> {
    /// Decoded visible text of the whole chunk.
    pub(crate) text: String,
    /// The source events in document order, for byte-preserving output.
    pub(crate) events: Vec<Event<'a>>,
    /// The first event after the chunk. The caller must handle it next.
    pub(crate) next: Event<'a>,
}

/// Collect `first` and every directly following text, reference or CDATA
/// event into one chunk.
pub(crate) fn read_text_chunk<'a>(
    first: Event<'a>,
    reader: &mut Reader<&'a [u8]>,
) -> Result<TextChunk<'a>> {
    let mut text = String::new();
    let mut events = Vec::new();
    let mut event = first;
    loop {
        match &event {
            Event::Text(value) => text.push_str(&value.html_content()),
            Event::CData(value) => text.push_str(value.as_ref()),
            Event::GeneralRef(reference) => text.push_str(&resolve_general_ref(reference)?),
            _ => {
                return Ok(TextChunk {
                    text,
                    events,
                    next: event,
                });
            }
        }
        events.push(event);
        event = reader.read_event()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adjacent_text_references_and_cdata_form_one_chunk() {
        let mut reader = Reader::from_str("<p>A &amp; B&#33;<![CDATA[ C]]><br/>D</p>");
        reader.config_mut().trim_text(false);
        assert!(matches!(reader.read_event().unwrap(), Event::Start(_)));

        let first = reader.read_event().unwrap();
        let chunk = read_text_chunk(first, &mut reader).unwrap();
        assert_eq!(chunk.text, "A & B! C");
        assert_eq!(chunk.events.len(), 5);
        assert!(matches!(chunk.next, Event::Empty(_)));

        let first = reader.read_event().unwrap();
        let chunk = read_text_chunk(first, &mut reader).unwrap();
        assert_eq!(chunk.text, "D");
        assert!(matches!(chunk.next, Event::End(_)));
    }

    #[test]
    fn comments_end_a_chunk() {
        let mut reader = Reader::from_str("<p>left<!--note-->right</p>");
        reader.config_mut().trim_text(false);
        assert!(matches!(reader.read_event().unwrap(), Event::Start(_)));

        let first = reader.read_event().unwrap();
        let chunk = read_text_chunk(first, &mut reader).unwrap();
        assert_eq!(chunk.text, "left");
        assert!(matches!(chunk.next, Event::Comment(_)));
    }
}
