//! Synthetic EPUB assembly from reconstructed blocks. The output is a
//! minimal, valid, reflowable EPUB 3 that the ordinary BookForge
//! pipeline (inspect, translate, validate, review) consumes unchanged.

use std::{fs::File, io::Write, path::Path};

use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

use crate::{
    Result,
    model::{DocBlock, Span},
};

const CONTAINER_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;

pub fn write_epub(blocks: &[DocBlock], title: &str, language: &str, output: &Path) -> Result<()> {
    let file = File::create(output)?;
    let mut zip = ZipWriter::new(file);
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    zip.start_file("mimetype", stored)?;
    zip.write_all(b"application/epub+zip")?;
    zip.start_file("META-INF/container.xml", deflated)?;
    zip.write_all(CONTAINER_XML.as_bytes())?;
    zip.start_file("content.opf", deflated)?;
    zip.write_all(opf(title, language).as_bytes())?;
    zip.start_file("content.xhtml", deflated)?;
    zip.write_all(chapter_xhtml(blocks, title).as_bytes())?;
    zip.finish()?;
    Ok(())
}

fn opf(title: &str, language: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="uid">bookforge-pdf-conversion</dc:identifier>
    <dc:title>{}</dc:title>
    <dc:language>{}</dc:language>
  </metadata>
  <manifest>
    <item id="content" href="content.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="content"/>
  </spine>
</package>"#,
        escape_text(title),
        escape_text(language)
    )
}

fn chapter_xhtml(blocks: &[DocBlock], title: &str) -> String {
    let mut body = String::new();
    for block in blocks {
        match block {
            DocBlock::Heading { level, spans } => {
                let level = (*level).clamp(1, 6);
                body.push_str(&format!("<h{level}>{}</h{level}>\n", render_spans(spans)));
            }
            DocBlock::Paragraph { spans } => {
                body.push_str(&format!("<p>{}</p>\n", render_spans(spans)));
            }
        }
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<head><title>{}</title></head>
<body>
{body}</body>
</html>"#,
        escape_text(title)
    )
}

fn render_spans(spans: &[Span]) -> String {
    let mut out = String::new();
    for span in spans {
        let text = escape_text(span.text.trim_matches('\u{0}'));
        match (span.bold, span.italic) {
            (true, true) => out.push_str(&format!("<b><i>{text}</i></b>")),
            (true, false) => out.push_str(&format!("<b>{text}</b>")),
            (false, true) => out.push_str(&format!("<i>{text}</i>")),
            (false, false) => out.push_str(&text),
        }
    }
    out.trim().to_string()
}

fn escape_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Span;

    fn span(text: &str) -> Span {
        Span {
            text: text.to_string(),
            bold: false,
            italic: false,
        }
    }

    #[test]
    fn produced_epub_is_readable_by_the_bookforge_reader() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("converted.epub");
        let blocks = vec![
            DocBlock::Heading {
                level: 1,
                spans: vec![span("Paper Title")],
            },
            DocBlock::Paragraph {
                spans: vec![
                    span("Body with "),
                    Span {
                        text: "emphasis".into(),
                        bold: false,
                        italic: true,
                    },
                    span(" & escapes <ok>."),
                ],
            },
        ];

        write_epub(&blocks, "Paper Title", "en", &path).expect("epub writes");

        let book = bookforge_epub::read_epub(&path).expect("bookforge must read its own output");
        assert!(
            book.blocks
                .iter()
                .any(|block| matches!(block.kind, bookforge_core::ir::BlockKind::Heading(1))),
            "heading must survive"
        );
        let coverage = bookforge_epub::text_coverage(&path).expect("coverage");
        assert_eq!(coverage.percent(), 100.0, "all text must be translatable");
    }
}
