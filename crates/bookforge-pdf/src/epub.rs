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

pub(crate) const MAX_CHAPTER_MATCHES: usize = 256;
pub(crate) const MIN_TEXT_BLOCKS_PER_MATCH: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChapterSplitOutcome {
    SingleChapter,
    Split { blocks_per_chapter: Vec<usize> },
    Guarded { matches: usize, text_blocks: usize },
}

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

pub(crate) fn write_epub_with_chapter_prefix(
    blocks: &[DocBlock],
    title: &str,
    language: &str,
    output: &Path,
    chapter_prefix: Option<&str>,
) -> Result<ChapterSplitOutcome> {
    let Some(chapter_prefix) = chapter_prefix else {
        write_epub(blocks, title, language, output)?;
        return Ok(ChapterSplitOutcome::SingleChapter);
    };
    let normalized_prefix = normalize_visible_text(chapter_prefix).to_lowercase();
    let text_blocks = blocks
        .iter()
        .filter(|block| !normalize_visible_text(&block.text()).is_empty())
        .count();
    let matches = blocks
        .iter()
        .enumerate()
        .filter_map(|(index, block)| {
            let text = normalize_visible_text(&block.text());
            (!text.is_empty() && text.to_lowercase().starts_with(&normalized_prefix))
                .then_some(index)
        })
        .collect::<Vec<_>>();

    if matches.is_empty() {
        write_epub(blocks, title, language, output)?;
        return Ok(ChapterSplitOutcome::SingleChapter);
    }
    if matches.len() > MAX_CHAPTER_MATCHES
        || matches.len().saturating_mul(MIN_TEXT_BLOCKS_PER_MATCH) > text_blocks
    {
        write_epub(blocks, title, language, output)?;
        return Ok(ChapterSplitOutcome::Guarded {
            matches: matches.len(),
            text_blocks,
        });
    }

    let has_front_matter = matches[0] != 0;
    let mut starts = Vec::with_capacity(matches.len() + 1);
    if has_front_matter {
        starts.push(0);
    }
    starts.extend(matches.iter().copied());
    if starts.len() < 2 {
        write_epub(blocks, title, language, output)?;
        return Ok(ChapterSplitOutcome::SingleChapter);
    }

    let chapters = starts
        .iter()
        .enumerate()
        .map(|(index, start)| {
            let end = starts.get(index + 1).copied().unwrap_or(blocks.len());
            let chapter_title = if index == 0 && has_front_matter {
                title.to_string()
            } else {
                normalize_visible_text(&blocks[*start].text())
            };
            EpubChapter {
                blocks: &blocks[*start..end],
                title: chapter_title,
            }
        })
        .collect::<Vec<_>>();
    let blocks_per_chapter = chapters
        .iter()
        .map(|chapter| chapter.blocks.len())
        .collect();
    write_multi_chapter_epub(blocks, &chapters, title, language, output)?;
    Ok(ChapterSplitOutcome::Split { blocks_per_chapter })
}

struct EpubChapter<'a> {
    blocks: &'a [DocBlock],
    title: String,
}

fn write_multi_chapter_epub(
    blocks: &[DocBlock],
    chapters: &[EpubChapter<'_>],
    title: &str,
    language: &str,
    output: &Path,
) -> Result<()> {
    let file = File::create(output)?;
    let mut zip = ZipWriter::new(file);
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    zip.start_file("mimetype", stored)?;
    zip.write_all(b"application/epub+zip")?;
    zip.start_file("META-INF/container.xml", deflated)?;
    zip.write_all(CONTAINER_XML.as_bytes())?;
    zip.start_file("content.opf", deflated)?;
    zip.write_all(multi_chapter_opf(title, language, chapters, figure_assets(blocks)).as_bytes())?;
    for (index, chapter) in chapters.iter().enumerate() {
        zip.start_file(chapter_href(index), deflated)?;
        zip.write_all(chapter_xhtml(chapter.blocks, &chapter.title).as_bytes())?;
    }
    zip.start_file("nav.xhtml", deflated)?;
    zip.write_all(multi_chapter_nav_xhtml(chapters).as_bytes())?;
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

fn multi_chapter_opf(
    title: &str,
    language: &str,
    chapters: &[EpubChapter<'_>],
    assets: Vec<&ImageAsset>,
) -> String {
    let chapter_items = chapters
        .iter()
        .enumerate()
        .map(|(index, _)| {
            format!(
                r#"    <item id="{}" href="{}" media-type="application/xhtml+xml"/>"#,
                chapter_id(index),
                chapter_href(index)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let spine_items = chapters
        .iter()
        .enumerate()
        .map(|(index, _)| format!(r#"    <itemref idref="{}"/>"#, chapter_id(index)))
        .collect::<Vec<_>>()
        .join("\n");
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
        .collect::<Vec<_>>();
    let asset_items = if asset_items.is_empty() {
        String::new()
    } else {
        format!("{}\n", asset_items.join("\n"))
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
{chapter_items}
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
{asset_items}  </manifest>
  <spine>
{spine_items}
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

fn multi_chapter_nav_xhtml(chapters: &[EpubChapter<'_>]) -> String {
    let entries = chapters
        .iter()
        .enumerate()
        .map(|(index, chapter)| {
            format!(
                r#"<li><a href="{}">{}</a></li>"#,
                chapter_href(index),
                escape_text(&chapter.title)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<head><title>Contents</title></head>
<body>
<nav epub:type="toc" id="toc">
<h1>Contents</h1>
<ol>{entries}</ol>
</nav>
</body>
</html>"#
    )
}

fn chapter_id(index: usize) -> String {
    format!("chapter-{:04}", index + 1)
}

fn chapter_href(index: usize) -> String {
    format!("chapter-{:04}.xhtml", index + 1)
}

fn normalize_visible_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
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

    fn paragraph(text: &str) -> DocBlock {
        DocBlock::Paragraph {
            spans: vec![span(text)],
        }
    }

    fn zip_entry(path: &Path, name: &str) -> String {
        let mut archive = ZipArchive::new(File::open(path).expect("epub opens")).expect("zip");
        let mut value = String::new();
        archive
            .by_name(name)
            .unwrap_or_else(|_| panic!("{name} exists"))
            .read_to_string(&mut value)
            .unwrap_or_else(|_| panic!("{name} reads"));
        value
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

    #[test]
    fn absent_chapter_prefix_is_byte_identical_to_the_legacy_writer() {
        let dir = tempfile::tempdir().expect("temp dir");
        let legacy = dir.path().join("legacy.epub");
        let optional = dir.path().join("optional.epub");
        let blocks = vec![
            paragraph("Preface"),
            paragraph("Chapter 1"),
            paragraph("Body"),
        ];

        write_epub(&blocks, "Book", "en", &legacy).expect("legacy EPUB writes");
        let outcome = write_epub_with_chapter_prefix(&blocks, "Book", "en", &optional, None)
            .expect("optional EPUB writes");

        assert_eq!(outcome, ChapterSplitOutcome::SingleChapter);
        assert_eq!(
            std::fs::read(legacy).expect("legacy EPUB reads"),
            std::fs::read(optional).expect("optional EPUB reads")
        );
    }

    #[test]
    fn chapter_prefix_splits_expected_boundaries_and_retains_front_matter() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("split.epub");
        let blocks = vec![
            DocBlock::Heading {
                level: 1,
                spans: vec![span("Book Title")],
            },
            paragraph("Prefatory matter"),
            paragraph("  Chapter   1: Origins  "),
            paragraph("First chapter body"),
            DocBlock::Heading {
                level: 2,
                spans: vec![span("CHAPTER 2: Consequences")],
            },
            paragraph("Second chapter body"),
            paragraph("Second chapter conclusion"),
        ];

        let outcome =
            write_epub_with_chapter_prefix(&blocks, "Book Title", "en", &path, Some("chapter "))
                .expect("split EPUB writes");

        assert_eq!(
            outcome,
            ChapterSplitOutcome::Split {
                blocks_per_chapter: vec![2, 2, 3]
            }
        );
        let book = bookforge_epub::read_epub(&path).expect("split EPUB reads");
        assert_eq!(book.spine.len(), 3);
        assert_eq!(
            book.sections
                .iter()
                .filter(|section| section.href.starts_with("chapter-"))
                .count(),
            3
        );
        assert_eq!(
            bookforge_epub::text_coverage(&path).unwrap().percent(),
            100.0
        );

        let front_matter = zip_entry(&path, "chapter-0001.xhtml");
        assert!(front_matter.contains("Prefatory matter"));
        assert!(!front_matter.contains("Chapter   1"));
        let first_chapter = zip_entry(&path, "chapter-0002.xhtml");
        assert!(first_chapter.contains("Chapter   1: Origins"));
        assert!(first_chapter.contains("First chapter body"));
        let nav = zip_entry(&path, "nav.xhtml");
        assert!(nav.contains("chapter-0001.xhtml\">Book Title</a>"));
        assert!(nav.contains("chapter-0002.xhtml\">Chapter 1: Origins</a>"));
        assert!(nav.contains("chapter-0003.xhtml\">CHAPTER 2: Consequences</a>"));
    }

    #[test]
    fn chapter_prefix_matching_nothing_uses_the_legacy_epub() {
        let dir = tempfile::tempdir().expect("temp dir");
        let legacy = dir.path().join("legacy.epub");
        let unmatched = dir.path().join("unmatched.epub");
        let blocks = vec![
            paragraph("Preface"),
            paragraph("Section One"),
            paragraph("Body"),
        ];

        write_epub(&blocks, "Book", "en", &legacy).expect("legacy EPUB writes");
        let outcome =
            write_epub_with_chapter_prefix(&blocks, "Book", "en", &unmatched, Some("Chapter "))
                .expect("unmatched EPUB writes");

        assert_eq!(outcome, ChapterSplitOutcome::SingleChapter);
        assert_eq!(
            std::fs::read(legacy).expect("legacy EPUB reads"),
            std::fs::read(unmatched).expect("unmatched EPUB reads")
        );
    }

    #[test]
    fn overmatching_chapter_prefix_is_guarded_by_the_legacy_epub() {
        let dir = tempfile::tempdir().expect("temp dir");
        let legacy = dir.path().join("legacy.epub");
        let guarded = dir.path().join("guarded.epub");
        let blocks = vec![
            paragraph("One"),
            paragraph("Two"),
            paragraph("Three"),
            paragraph("Four"),
        ];

        write_epub(&blocks, "Book", "en", &legacy).expect("legacy EPUB writes");
        let outcome = write_epub_with_chapter_prefix(&blocks, "Book", "en", &guarded, Some(""))
            .expect("guarded EPUB writes");

        assert_eq!(
            outcome,
            ChapterSplitOutcome::Guarded {
                matches: 4,
                text_blocks: 4
            }
        );
        assert_eq!(
            std::fs::read(legacy).expect("legacy EPUB reads"),
            std::fs::read(guarded).expect("guarded EPUB reads")
        );
    }
}
