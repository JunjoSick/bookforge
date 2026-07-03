//! Synthetic EPUB assembly from reconstructed blocks. The output is a
//! minimal, valid, reflowable EPUB 3 that the ordinary BookForge
//! pipeline (inspect, translate, validate, review) consumes unchanged.

use std::{fs::File, io::Write, path::Path};

use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

use crate::{
    Result,
    model::{DocBlock, ImageAsset, Span},
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
    zip.write_all(opf(title, language, figure_assets(blocks)).as_bytes())?;
    zip.start_file("content.xhtml", deflated)?;
    zip.write_all(chapter_xhtml(blocks, title).as_bytes())?;
    zip.start_file("nav.xhtml", deflated)?;
    zip.write_all(nav_xhtml(title).as_bytes())?;
    for asset in figure_assets(blocks) {
        zip.start_file(asset.href.as_str(), deflated)?;
        zip.write_all(&asset.bytes)?;
    }
    zip.finish()?;
    Ok(())
}

fn opf(title: &str, language: &str, assets: Vec<&ImageAsset>) -> String {
    let asset_items = assets
        .iter()
        .map(|asset| {
            format!(
                r#"    <item id="{}" href="{}" media-type="{}"/>"#,
                escape_attr(&asset.id),
                escape_attr(&asset.href),
                escape_attr(&asset.media_type)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let asset_items = if asset_items.is_empty() {
        String::new()
    } else {
        format!("{asset_items}\n")
    };

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="uid">bookforge-pdf-conversion</dc:identifier>
    <dc:title>{}</dc:title>
    <dc:language>{}</dc:language>
    <meta property="dcterms:modified">1970-01-01T00:00:00Z</meta>
  </metadata>
  <manifest>
    <item id="content" href="content.xhtml" media-type="application/xhtml+xml"/>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
{asset_items}
  </manifest>
  <spine>
    <itemref idref="content"/>
  </spine>
</package>"#,
        escape_text(title),
        escape_text(language)
    )
}

fn nav_xhtml(title: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<head><title>Contents</title></head>
<body>
<nav epub:type="toc" id="toc">
<h1>Contents</h1>
<ol><li><a href="content.xhtml">{}</a></li></ol>
</nav>
</body>
</html>"#,
        escape_text(title)
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
            DocBlock::Figure { image, caption } => {
                body.push_str(&format!(
                    "<figure id=\"{}\"><img src=\"{}\" alt=\"PDF image from page {}\"/>",
                    escape_attr(&image.id),
                    escape_attr(&image.href),
                    image.page
                ));
                if let Some(caption) = caption {
                    body.push_str(&format!(
                        "<figcaption>{}</figcaption>",
                        render_spans(caption)
                    ));
                }
                body.push_str("</figure>\n");
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

fn figure_assets(blocks: &[DocBlock]) -> Vec<&ImageAsset> {
    blocks
        .iter()
        .filter_map(|block| match block {
            DocBlock::Figure { image, .. } => Some(image),
            _ => None,
        })
        .collect()
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

fn escape_attr(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Span;
    use std::io::Read;
    use zip::ZipArchive;

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

        let mut archive = ZipArchive::new(File::open(&path).expect("epub opens")).expect("zip");
        let mut opf = String::new();
        archive
            .by_name("content.opf")
            .expect("opf exists")
            .read_to_string(&mut opf)
            .expect("opf reads");
        assert!(opf.contains("property=\"dcterms:modified\""));
        assert!(opf.contains("properties=\"nav\""));
        let mut nav = String::new();
        archive
            .by_name("nav.xhtml")
            .expect("nav exists")
            .read_to_string(&mut nav)
            .expect("nav reads");
        assert!(nav.contains("epub:type=\"toc\""));
        assert!(nav.contains("<a href=\"content.xhtml\">Paper Title</a>"));
    }
}
