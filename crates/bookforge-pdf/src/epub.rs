//! Synthetic EPUB assembly from reconstructed blocks. The output is a
//! minimal, valid, reflowable EPUB 3 that the ordinary BookForge
//! pipeline (inspect, translate, validate, review) consumes unchanged.
//!
//! Determinism conventions match the rest of the workspace
//! (`bookforge-epub` writer): zip entry timestamps are pinned to the
//! fixed DOS epoch, the unique identifier derives from the source
//! bytes, and `dcterms:modified` honors `SOURCE_DATE_EPOCH` with the
//! same epoch constant as fallback — wall-clock time never enters the
//! output.

use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use zip::{CompressionMethod, DateTime, ZipArchive, ZipWriter, write::SimpleFileOptions};

use crate::{
    Result,
    model::{DocBlock, ImageAsset, Span, spans_text},
};

const CONTAINER_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;

/// The workspace determinism convention for archive timestamps
/// (mirrors `bookforge_epub::util::deterministic_zip_time`): byte-for-
/// byte reproducible conversions regardless of when they run.
pub(crate) const DETERMINISTIC_MODIFIED: &str = "1980-01-01T00:00:00Z";

pub(crate) const MAX_CHAPTER_MATCHES: usize = 256;
pub(crate) const MIN_TEXT_BLOCKS_PER_MATCH: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChapterSplitOutcome {
    SingleChapter,
    Split { blocks_per_chapter: Vec<usize> },
    Guarded { matches: usize, text_blocks: usize },
}

/// `dcterms:modified` source consistent with the workspace conventions:
/// `SOURCE_DATE_EPOCH` (the reproducible-builds convention, seconds
/// since the Unix epoch) when set to a usable value, otherwise the
/// fixed epoch constant. Wall-clock time is deliberately never used —
/// it would break byte-for-byte reproducibility.
pub(crate) fn publication_timestamp() -> String {
    std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .and_then(rfc3339_from_unix_seconds)
        .unwrap_or_else(|| DETERMINISTIC_MODIFIED.to_string())
}

/// Convert Unix seconds to an xsd/RFC3339 UTC timestamp
/// ("YYYY-MM-DDTHH:MM:SSZ"), clamped into years [1970, 9999].
fn rfc3339_from_unix_seconds(seconds: u64) -> Option<String> {
    let days = seconds / 86_400;
    let seconds_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days)?;
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds_of_day / 3600,
        (seconds_of_day % 3600) / 60,
        seconds_of_day % 60
    ))
}

/// Days since 1970-01-01 → proleptic Gregorian (year, month, day).
fn civil_from_days(days: u64) -> Option<(i64, u32, u32)> {
    if days > 2_932_896 {
        return None; // year 9999 safety ceiling
    }
    // Howard Hinnant's civil-from-days algorithm.
    let z = i64::try_from(days).ok()? + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    Some((year, m as u32, d as u32))
}

fn pinned_zip_options(compression: CompressionMethod) -> SimpleFileOptions {
    let time =
        DateTime::from_date_and_time(1980, 1, 1, 0, 0, 0).expect("DOS epoch is a valid date");
    SimpleFileOptions::default()
        .compression_method(compression)
        .last_modified_time(time)
}

pub fn write_epub(
    blocks: &[DocBlock],
    title: &str,
    language: &str,
    output: &Path,
    source_id: &str,
    modified: &str,
) -> Result<()> {
    let (staged, file) = create_sibling_file(output, "pdf-epub")?;
    let result = write_epub_file(file, blocks, title, language, source_id, modified)
        .and_then(|()| validate_written_epub(&staged));
    if let Err(error) = result {
        let _ = fs::remove_file(&staged);
        return Err(error);
    }
    match publish_staged(&staged, output) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&staged);
            Err(error)
        }
    }
}

fn write_epub_file(
    file: File,
    blocks: &[DocBlock],
    title: &str,
    language: &str,
    source_id: &str,
    modified: &str,
) -> Result<()> {
    let mut zip = ZipWriter::new(file);
    let stored = pinned_zip_options(CompressionMethod::Stored);
    let deflated = pinned_zip_options(CompressionMethod::Deflated);

    zip.start_file("mimetype", stored)?;
    zip.write_all(b"application/epub+zip")?;
    zip.start_file("META-INF/container.xml", deflated)?;
    zip.write_all(CONTAINER_XML.as_bytes())?;
    zip.start_file("content.opf", deflated)?;
    zip.write_all(opf(title, language, source_id, modified, figure_assets(blocks)).as_bytes())?;
    zip.start_file("content.xhtml", deflated)?;
    zip.write_all(chapter_xhtml(blocks, title, 0).as_bytes())?;
    zip.start_file("nav.xhtml", deflated)?;
    let headings = detected_headings(blocks, 0);
    zip.write_all(nav_xhtml(title, &headings).as_bytes())?;
    for asset in figure_assets(blocks) {
        zip.start_file(asset.href.as_str(), deflated)?;
        zip.write_all(&asset.bytes)?;
    }
    zip.finish()?;
    Ok(())
}

/// Write the EPUB as one XHTML file per chapter. With `chapter_prefix`,
/// a chapter starts at every text block that begins with it; without
/// one, at every level-1 heading. Blocks before the first start are kept
/// as a front-matter chapter. When nothing splits, or the split guard
/// trips, the single `content.xhtml` writer is used unchanged.
pub(crate) fn write_chaptered_epub(
    blocks: &[DocBlock],
    title: &str,
    language: &str,
    output: &Path,
    chapter_prefix: Option<&str>,
    source_id: &str,
    modified: &str,
) -> Result<ChapterSplitOutcome> {
    let text_blocks = blocks
        .iter()
        .filter(|block| !normalize_visible_text(&block.text()).is_empty())
        .count();
    let matches = match chapter_prefix {
        Some(prefix) => prefix_matches(blocks, prefix),
        None => level_one_heading_matches(blocks),
    };

    if matches.is_empty() {
        write_epub(blocks, title, language, output, source_id, modified)?;
        return Ok(ChapterSplitOutcome::SingleChapter);
    }
    if matches.len() > MAX_CHAPTER_MATCHES
        || matches.len().saturating_mul(MIN_TEXT_BLOCKS_PER_MATCH) > text_blocks
    {
        write_epub(blocks, title, language, output, source_id, modified)?;
        return Ok(ChapterSplitOutcome::Guarded {
            matches: matches.len(),
            text_blocks,
        });
    }

    let starts = match chapter_prefix {
        Some(_) => prefix_chapter_starts(&matches),
        None => heading_chapter_starts(blocks, &matches),
    };
    if starts.len() < 2 {
        write_epub(blocks, title, language, output, source_id, modified)?;
        return Ok(ChapterSplitOutcome::SingleChapter);
    }

    let mut first_heading = 0;
    let chapters = starts
        .iter()
        .enumerate()
        .map(|(index, &start)| {
            let end = starts.get(index + 1).copied().unwrap_or(blocks.len());
            // A chapter is named by the first boundary inside it; only the
            // front matter has none and takes the book title.
            let first_match = matches.partition_point(|&found| found < start);
            let chapter_title = matches
                .get(first_match)
                .filter(|&&found| found < end)
                .map(|&found| normalize_visible_text(&blocks[found].text()))
                .unwrap_or_else(|| title.to_string());
            let chapter = EpubChapter {
                blocks: &blocks[start..end],
                title: chapter_title,
                first_heading,
            };
            first_heading += chapter
                .blocks
                .iter()
                .filter(|block| matches!(block, DocBlock::Heading { .. }))
                .count();
            chapter
        })
        .collect::<Vec<_>>();
    let blocks_per_chapter = chapters
        .iter()
        .map(|chapter| chapter.blocks.len())
        .collect();
    write_multi_chapter_epub(
        blocks, &chapters, title, language, output, source_id, modified,
    )?;
    Ok(ChapterSplitOutcome::Split { blocks_per_chapter })
}

fn prefix_matches(blocks: &[DocBlock], chapter_prefix: &str) -> Vec<usize> {
    let normalized_prefix = normalize_visible_text(chapter_prefix).to_lowercase();
    blocks
        .iter()
        .enumerate()
        .filter_map(|(index, block)| {
            let text = normalize_visible_text(&block.text());
            (!text.is_empty() && text.to_lowercase().starts_with(&normalized_prefix))
                .then_some(index)
        })
        .collect()
}

fn level_one_heading_matches(blocks: &[DocBlock]) -> Vec<usize> {
    blocks
        .iter()
        .enumerate()
        .filter_map(|(index, block)| match block {
            DocBlock::Heading { level: 1, spans }
                if !normalize_visible_text(&spans_text(spans)).is_empty() =>
            {
                Some(index)
            }
            _ => None,
        })
        .collect()
}

/// Every prefix match starts a chapter, behind a front-matter chapter
/// when the first match is not the first block.
fn prefix_chapter_starts(matches: &[usize]) -> Vec<usize> {
    let mut starts = Vec::with_capacity(matches.len() + 1);
    if matches[0] != 0 {
        starts.push(0);
    }
    starts.extend(matches.iter().copied());
    starts
}

/// Chapter starts for the default level-1 heading split. A chapter that
/// would hold nothing but headings (a half-title page ahead of the title
/// page, a part title ahead of its first chapter) merges into the next
/// one, and a heading-only tail merges into the one before. Front matter
/// keeps a chapter of its own only when it has body content; otherwise
/// it opens the first chapter. Fewer than two chapters with body content
/// means there is nothing to split: a paper whose only level-1 heading is
/// its title stays one file.
fn heading_chapter_starts(blocks: &[DocBlock], matches: &[usize]) -> Vec<usize> {
    let mut starts = Vec::with_capacity(matches.len() + 1);
    let mut heading_only_start = None;
    for (index, &start) in matches.iter().enumerate() {
        let end = matches.get(index + 1).copied().unwrap_or(blocks.len());
        let chapter_start = heading_only_start.take().unwrap_or(start);
        if blocks[start..end].iter().any(is_body_block) {
            starts.push(chapter_start);
        } else {
            heading_only_start = Some(chapter_start);
        }
    }
    if starts.len() < 2 {
        return starts;
    }
    if blocks[..matches[0]].iter().any(is_body_block) {
        starts.insert(0, 0);
    } else {
        starts[0] = 0;
    }
    starts
}

/// Content a chapter file must hold besides its headings: visible prose
/// or a figure.
fn is_body_block(block: &DocBlock) -> bool {
    match block {
        DocBlock::Heading { .. } => false,
        DocBlock::Paragraph { spans } => !normalize_visible_text(&spans_text(spans)).is_empty(),
        DocBlock::Figure { .. } => true,
    }
}

struct EpubChapter<'a> {
    blocks: &'a [DocBlock],
    title: String,
    /// Ordinal of the chapter's first heading in the whole book, so
    /// heading ids stay unique across files and match the single-file
    /// numbering.
    first_heading: usize,
}

fn write_multi_chapter_epub(
    blocks: &[DocBlock],
    chapters: &[EpubChapter<'_>],
    title: &str,
    language: &str,
    output: &Path,
    source_id: &str,
    modified: &str,
) -> Result<()> {
    let (staged, file) = create_sibling_file(output, "pdf-epub")?;
    let result =
        write_multi_chapter_epub_file(file, blocks, chapters, title, language, source_id, modified)
            .and_then(|()| validate_written_epub(&staged));
    if let Err(error) = result {
        let _ = fs::remove_file(&staged);
        return Err(error);
    }
    match publish_staged(&staged, output) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&staged);
            Err(error)
        }
    }
}

fn write_multi_chapter_epub_file(
    file: File,
    blocks: &[DocBlock],
    chapters: &[EpubChapter<'_>],
    title: &str,
    language: &str,
    source_id: &str,
    modified: &str,
) -> Result<()> {
    let mut zip = ZipWriter::new(file);
    let stored = pinned_zip_options(CompressionMethod::Stored);
    let deflated = pinned_zip_options(CompressionMethod::Deflated);

    zip.start_file("mimetype", stored)?;
    zip.write_all(b"application/epub+zip")?;
    zip.start_file("META-INF/container.xml", deflated)?;
    zip.write_all(CONTAINER_XML.as_bytes())?;
    zip.start_file("content.opf", deflated)?;
    zip.write_all(
        multi_chapter_opf(
            title,
            language,
            source_id,
            modified,
            chapters,
            figure_assets(blocks),
        )
        .as_bytes(),
    )?;
    for (index, chapter) in chapters.iter().enumerate() {
        zip.start_file(chapter_href(index), deflated)?;
        zip.write_all(
            chapter_xhtml(chapter.blocks, &chapter.title, chapter.first_heading).as_bytes(),
        )?;
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

fn create_sibling_file(output: &Path, label: &str) -> Result<(PathBuf, File)> {
    let name = output
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("book.epub");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for attempt in 0..128u32 {
        let path = output.with_file_name(format!(
            ".{name}.bookforge-{label}-{}-{nonce}-{attempt}",
            std::process::id()
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(crate::PdfError::InvalidInput(format!(
        "could not reserve a unique temporary EPUB path beside {}",
        output.display()
    )))
}

#[cfg(unix)]
fn publish_staged(staged: &Path, output: &Path) -> Result<()> {
    // On Unix, rename replaces the destination as one directory transaction.
    // The old artifact is untouched until the staged EPUB has been completely
    // written and validated above.
    fs::rename(staged, output)?;
    Ok(())
}

#[cfg(not(unix))]
fn publish_staged(staged: &Path, output: &Path) -> Result<()> {
    // Windows cannot atomically rename over an existing file. Move the old
    // good artifact aside only after the replacement is complete, restoring it
    // if the second rename fails.
    if !output.exists() {
        fs::rename(staged, output)?;
        return Ok(());
    }

    let backup = output.with_file_name(format!(
        ".{}.bookforge-backup-{}",
        output
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("book.epub"),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    fs::rename(output, &backup)?;
    match fs::rename(staged, output) {
        Ok(()) => {
            let _ = fs::remove_file(backup);
            Ok(())
        }
        Err(error) => {
            let _ = fs::rename(&backup, output);
            Err(error.into())
        }
    }
}

fn validate_written_epub(path: &Path) -> Result<()> {
    let mut archive = ZipArchive::new(File::open(path)?)?;
    let mut mimetype = Vec::new();
    archive.by_name("mimetype")?.read_to_end(&mut mimetype)?;
    if mimetype != b"application/epub+zip" {
        return Err(crate::PdfError::InvalidInput(
            "generated EPUB has an invalid mimetype entry".to_string(),
        ));
    }
    for name in ["META-INF/container.xml", "content.opf", "nav.xhtml"] {
        archive.by_name(name)?;
    }
    Ok(())
}

fn opf(
    title: &str,
    language: &str,
    source_id: &str,
    modified: &str,
    assets: Vec<&ImageAsset>,
) -> String {
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
    <dc:identifier id="uid">{}</dc:identifier>
    <dc:title>{}</dc:title>
    <dc:language>{}</dc:language>
    <meta property="dcterms:modified">{}</meta>
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
        escape_attr(source_id),
        escape_text(title),
        escape_text(language),
        escape_text(modified),
    )
}

fn multi_chapter_opf(
    title: &str,
    language: &str,
    source_id: &str,
    modified: &str,
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
    <dc:identifier id="uid">{}</dc:identifier>
    <dc:title>{}</dc:title>
    <dc:language>{}</dc:language>
    <meta property="dcterms:modified">{}</meta>
  </metadata>
  <manifest>
{chapter_items}
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
{asset_items}  </manifest>
  <spine>
{spine_items}
  </spine>
</package>"#,
        escape_attr(source_id),
        escape_text(title),
        escape_text(language),
        escape_text(modified),
    )
}

/// Detected headings (level ≤ 3) usable as table-of-contents entries,
/// in document order with their heading ordinals, counted from
/// `first_ordinal` exactly as `chapter_xhtml` numbers its `head-NNNN`
/// ids.
fn detected_headings(blocks: &[DocBlock], first_ordinal: usize) -> Vec<(u8, String, usize)> {
    let mut headings = Vec::new();
    let mut ordinal = first_ordinal;
    for block in blocks {
        if let DocBlock::Heading { level, spans } = block {
            if *level <= 3 {
                let label = normalize_visible_text(&spans_text(spans));
                if !label.is_empty() {
                    headings.push((*level, label, ordinal));
                }
            }
            ordinal += 1;
        }
    }
    headings
}

fn heading_anchor(ordinal: usize) -> String {
    format!("head-{ordinal:04}")
}

/// Build nested `<ol>` markup for table-of-contents entries that link
/// into `file`. Levels are normalized to nesting depths first: a heading
/// nests under the nearest earlier heading of a higher rank and is a
/// sibling of one at its own rank, so the first entry is at depth 1 and
/// no entry opens more than one list below its predecessor even when a
/// rank is skipped (h1 then h3). The depths are then rendered
/// recursively so every produced XHTML list nests correctly.
fn heading_toc_markup(file: &str, headings: &[(u8, String, usize)]) -> String {
    if headings.is_empty() {
        return String::new();
    }
    let mut items: Vec<(u8, String, usize)> = Vec::with_capacity(headings.len());
    let mut open_levels: Vec<u8> = Vec::new();
    for (level, label, ordinal) in headings {
        let level = (*level).clamp(1, 3);
        while open_levels.last().is_some_and(|&open| open >= level) {
            open_levels.pop();
        }
        open_levels.push(level);
        items.push((open_levels.len() as u8, label.clone(), *ordinal));
    }

    let mut out = String::new();
    write_toc_list(&mut out, file, &items);
    out
}

/// Precondition: `items` is non-empty. `items[*].0` is the nesting
/// depth relative to this call's own base level (the first item).
fn write_toc_list(out: &mut String, file: &str, items: &[(u8, String, usize)]) {
    // Rebase depths so the block starts at 1.
    let base = items[0].0 - 1;
    out.push_str("<ol>");
    let mut index = 0usize;
    while index < items.len() {
        let (raw_depth, label, ordinal) = &items[index];
        let depth = raw_depth - base;
        debug_assert_eq!(depth, 1, "block leaders are rebased to depth 1");
        out.push_str(&format!(
            r#"<li><a href="{file}#{}">{}</a>"#,
            heading_anchor(*ordinal),
            escape_text(label)
        ));
        let mut end = index + 1;
        while end < items.len() && items[end].0 > *raw_depth {
            end += 1;
        }
        if end > index + 1 {
            write_toc_list(out, file, &items[index + 1..end]);
        }
        out.push_str("</li>");
        index = end;
    }
    out.push_str("</ol>");
}

fn nav_xhtml(title: &str, headings: &[(u8, String, usize)]) -> String {
    // TOC is built from the detected headings when there are any; the
    // title-only single entry remains the fallback otherwise.
    let toc_entries = if headings.is_empty() {
        format!(
            r#"<li><a href="content.xhtml">{}</a></li>"#,
            escape_text(title)
        )
    } else {
        heading_toc_markup("content.xhtml", headings)
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<head><title>Contents</title></head>
<body>
<nav epub:type="toc" id="toc">
<h1>Contents</h1>
{toc_entries}
</nav>
</body>
</html>"#
    )
}

/// One entry per chapter file, with the chapter's own h1–h3 headings
/// nested under it and linked into that file.
fn multi_chapter_nav_xhtml(chapters: &[EpubChapter<'_>]) -> String {
    let entries = chapters
        .iter()
        .enumerate()
        .map(|(index, chapter)| {
            let href = chapter_href(index);
            format!(
                r#"<li><a href="{href}">{}</a>{}</li>"#,
                escape_text(&chapter.title),
                heading_toc_markup(&href, &chapter_toc_headings(chapter))
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

/// The headings to nest under a chapter's nav entry. A heading ahead of
/// the chapter's first body content that repeats the chapter title is
/// the entry itself (or a half-title page merged in ahead of the title
/// page), so it is left out rather than listed under its own name.
fn chapter_toc_headings(chapter: &EpubChapter<'_>) -> Vec<(u8, String, usize)> {
    let opening_headings = chapter
        .blocks
        .iter()
        .take_while(|block| !is_body_block(block))
        .filter(|block| matches!(block, DocBlock::Heading { .. }))
        .count();
    let opening_end = chapter.first_heading + opening_headings;
    let title = chapter.title.to_lowercase();
    detected_headings(chapter.blocks, chapter.first_heading)
        .into_iter()
        .filter(|(_, label, ordinal)| !(*ordinal < opening_end && label.to_lowercase() == title))
        .collect()
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

fn chapter_xhtml(blocks: &[DocBlock], title: &str, first_heading: usize) -> String {
    let mut body = String::new();
    // Heading anchors mirror `detected_headings` numbering: every
    // Heading block gets the next ordinal, so TOC links stay valid
    // regardless of level filtering.
    let mut ordinal = first_heading;
    for block in blocks {
        match block {
            DocBlock::Heading { level, spans } => {
                let level = (*level).clamp(1, 6);
                body.push_str(&format!(
                    "<h{level} id=\"{}\">{}</h{level}>\n",
                    heading_anchor(ordinal),
                    render_spans(spans)
                ));
                ordinal += 1;
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
        let styled = match (span.bold, span.italic) {
            (true, true) => format!("<b><i>{text}</i></b>"),
            (true, false) => format!("<b>{text}</b>"),
            (false, true) => format!("<i>{text}</i>"),
            (false, false) => text,
        };
        if span.superscript {
            out.push_str(&format!("<sup>{styled}</sup>"));
        } else {
            out.push_str(&styled);
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
            superscript: false,
        }
    }

    fn paragraph(text: &str) -> DocBlock {
        DocBlock::Paragraph {
            spans: vec![span(text)],
        }
    }

    fn heading(level: u8, text: &str) -> DocBlock {
        DocBlock::Heading {
            level,
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

    fn zip_names(path: &Path) -> Vec<String> {
        let archive = ZipArchive::new(File::open(path).expect("epub opens")).expect("zip");
        archive
            .file_names()
            .map(|name| name.expect("zip entry name").into_owned())
            .collect()
    }

    /// Every nav link must name a file in the archive and, when it has a
    /// fragment, an id inside that file. Returns the links checked.
    fn assert_nav_links_resolve(path: &Path) -> Vec<String> {
        let nav = zip_entry(path, "nav.xhtml");
        let links = nav
            .split("href=\"")
            .skip(1)
            .map(|rest| rest[..rest.find('"').expect("closed href")].to_string())
            .collect::<Vec<_>>();
        assert!(!links.is_empty(), "nav has links: {nav}");
        for link in &links {
            let (file, fragment) = link.split_once('#').unwrap_or((link, ""));
            let target = zip_entry(path, file);
            if !fragment.is_empty() {
                assert!(
                    target.contains(&format!("id=\"{fragment}\"")),
                    "nav link {link} must resolve inside {file}: {target}"
                );
            }
        }
        links
    }

    #[test]
    fn superscript_spans_render_as_sup() {
        let mark = Span {
            text: "87".to_string(),
            bold: false,
            italic: true,
            superscript: true,
        };
        assert_eq!(
            render_spans(&[span("it cites"), mark, span(" and goes on.")]),
            "it cites<sup><i>87</i></sup> and goes on."
        );
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
                        superscript: false,
                    },
                    span(" & escapes <ok>."),
                ],
            },
            DocBlock::Heading {
                level: 2,
                spans: vec![span("Section One")],
            },
            DocBlock::Heading {
                level: 3,
                spans: vec![span("Subsection A")],
            },
        ];

        write_epub(
            &blocks,
            "Paper Title",
            "en",
            &path,
            "bookforge-pdf-1234-abcdbeef",
            DETERMINISTIC_MODIFIED,
        )
        .expect("epub writes");

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
        assert!(opf.contains(DETERMINISTIC_MODIFIED));
        assert!(
            opf.contains("<dc:identifier id=\"uid\">bookforge-pdf-1234-abcdbeef</dc:identifier>")
        );
        assert!(opf.contains("properties=\"nav\""));
        let mut nav = String::new();
        archive
            .by_name("nav.xhtml")
            .expect("nav exists")
            .read_to_string(&mut nav)
            .expect("nav reads");
        assert!(nav.contains("epub:type=\"toc\""));
        assert!(
            nav.contains("<a href=\"content.xhtml#head-0000\">Paper Title</a>"),
            "TOC must be built from detected headings: {nav}"
        );
        assert!(nav.contains("<a href=\"content.xhtml#head-0002\">Subsection A</a>"));

        let mut content = String::new();
        archive
            .by_name("content.xhtml")
            .expect("content exists")
            .read_to_string(&mut content)
            .expect("content reads");
        assert!(content.contains("<h1 id=\"head-0000\">"), "{content}");
        assert!(content.contains("<h3 id=\"head-0002\">"), "{content}");
    }

    #[test]
    fn failed_publication_restores_existing_epub() {
        let dir = tempfile::tempdir().expect("temp dir");
        let output = dir.path().join("existing.epub");
        let staged = dir.path().join("missing-stage.epub");
        std::fs::write(&output, b"known-good").expect("existing EPUB writes");

        let error = publish_staged(&staged, &output).expect_err("missing stage must fail");

        assert!(!error.to_string().is_empty());
        assert_eq!(
            std::fs::read(&output).expect("existing EPUB remains"),
            b"known-good"
        );
    }

    #[test]
    fn nav_falls_back_to_title_only_without_headings() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("fallback.epub");
        let blocks = vec![paragraph("Plain prose only.")];

        write_epub(
            &blocks,
            "Title",
            "en",
            &path,
            "bookforge-pdf-fallback",
            DETERMINISTIC_MODIFIED,
        )
        .expect("epub writes");

        let nav = zip_entry(&path, "nav.xhtml");
        assert!(nav.contains("<a href=\"content.xhtml\">Title</a>"));
        assert!(!nav.contains("#head-"), "no anchors without headings");
    }

    #[test]
    fn identical_sources_produce_byte_identical_epubs() {
        let dir = tempfile::tempdir().expect("temp dir");
        let first = dir.path().join("first.epub");
        let second = dir.path().join("second.epub");
        let blocks = vec![
            DocBlock::Heading {
                level: 1,
                spans: vec![span("Determinism")],
            },
            paragraph("Body text."),
        ];

        for path in [&first, &second] {
            write_epub(
                &blocks,
                "Determinism",
                "en",
                path,
                "bookforge-pdf-same-source",
                DETERMINISTIC_MODIFIED,
            )
            .expect("epub writes");
        }

        assert_eq!(
            std::fs::read(&first).expect("first reads"),
            std::fs::read(&second).expect("second reads"),
            "same inputs must yield byte-identical EPUBs (zip timestamps pinned)"
        );
    }

    #[test]
    fn publication_timestamp_honors_source_date_epoch_and_falls_back_to_the_dos_epoch() {
        unsafe { std::env::remove_var("SOURCE_DATE_EPOCH") };
        assert_eq!(publication_timestamp(), DETERMINISTIC_MODIFIED);

        unsafe { std::env::set_var("SOURCE_DATE_EPOCH", "1709164800") };
        let stamp = publication_timestamp();
        unsafe { std::env::remove_var("SOURCE_DATE_EPOCH") };
        assert_eq!(stamp, "2024-02-29T00:00:00Z");

        // Invalid values fall back instead of panicking or leaking wall time.
        unsafe { std::env::set_var("SOURCE_DATE_EPOCH", "not-a-number") };
        assert_eq!(publication_timestamp(), DETERMINISTIC_MODIFIED);
        unsafe { std::env::remove_var("SOURCE_DATE_EPOCH") };
    }

    #[test]
    fn unix_seconds_convert_to_rfc3339() {
        assert_eq!(
            rfc3339_from_unix_seconds(0).as_deref(),
            Some("1970-01-01T00:00:00Z")
        );
        assert_eq!(
            rfc3339_from_unix_seconds(951_782_400).as_deref(),
            Some("2000-02-29T00:00:00Z")
        );
        assert_eq!(
            rfc3339_from_unix_seconds(2_145_916_800).as_deref(),
            Some("2038-01-01T00:00:00Z")
        );
        assert_eq!(
            rfc3339_from_unix_seconds(1_700_000_000).as_deref(),
            Some("2023-11-14T22:13:20Z")
        );
        assert!(rfc3339_from_unix_seconds(u64::MAX).is_none());
    }

    #[test]
    fn heading_toc_markup_nests_and_escapes() {
        let headings = vec![
            (1u8, "Intro & overview".to_string(), 0),
            (2u8, "Background".to_string(), 1),
            (5u8, "Clamped deep <level>".to_string(), 2),
            (1u8, "Next chapter".to_string(), 3),
        ];

        let markup = heading_toc_markup("content.xhtml", &headings);

        // A level-5 heading clamps to depth 3, so it nests under the
        // level-2 section rather than producing an invalid sibling jump.
        assert_eq!(
            markup,
            "<ol><li><a href=\"content.xhtml#head-0000\">Intro &amp; overview</a><ol><li><a href=\"content.xhtml#head-0001\">Background</a><ol><li><a href=\"content.xhtml#head-0002\">Clamped deep &lt;level&gt;</a></li></ol></li></ol></li><li><a href=\"content.xhtml#head-0003\">Next chapter</a></li></ol>"
        );
    }

    #[test]
    fn headings_of_one_rank_stay_siblings_after_a_skipped_rank() {
        // h1 then h3 sections (no h2): the sections nest one list down and
        // stay siblings instead of each opening a list under the last.
        let headings = vec![
            (1u8, "Chapter".to_string(), 0),
            (3u8, "1.1 First".to_string(), 1),
            (3u8, "1.2 Second".to_string(), 2),
            (2u8, "Part".to_string(), 3),
            (3u8, "1.3 Third".to_string(), 4),
        ];

        assert_eq!(
            heading_toc_markup("f.xhtml", &headings),
            "<ol><li><a href=\"f.xhtml#head-0000\">Chapter</a><ol><li><a href=\"f.xhtml#head-0001\">1.1 First</a></li><li><a href=\"f.xhtml#head-0002\">1.2 Second</a></li><li><a href=\"f.xhtml#head-0003\">Part</a><ol><li><a href=\"f.xhtml#head-0004\">1.3 Third</a></li></ol></li></ol></li></ol>"
        );
        // Sections leading a chapter's nav children are siblings too.
        assert_eq!(
            heading_toc_markup("f.xhtml", &headings[1..3]),
            "<ol><li><a href=\"f.xhtml#head-0001\">1.1 First</a></li><li><a href=\"f.xhtml#head-0002\">1.2 Second</a></li></ol>"
        );
    }

    #[test]
    fn absent_chapter_prefix_without_headings_is_byte_identical_to_the_legacy_writer() {
        let dir = tempfile::tempdir().expect("temp dir");
        let legacy = dir.path().join("legacy.epub");
        let optional = dir.path().join("optional.epub");
        let blocks = vec![
            paragraph("Preface"),
            paragraph("Chapter 1"),
            paragraph("Body"),
        ];

        write_epub(
            &blocks,
            "Book",
            "en",
            &legacy,
            "test-source",
            DETERMINISTIC_MODIFIED,
        )
        .expect("legacy EPUB writes");
        let outcome = write_chaptered_epub(
            &blocks,
            "Book",
            "en",
            &optional,
            None,
            "test-source",
            DETERMINISTIC_MODIFIED,
        )
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

        let outcome = write_chaptered_epub(
            &blocks,
            "Book Title",
            "en",
            &path,
            Some("chapter "),
            "test-source",
            DETERMINISTIC_MODIFIED,
        )
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

        write_epub(
            &blocks,
            "Book",
            "en",
            &legacy,
            "test-source",
            DETERMINISTIC_MODIFIED,
        )
        .expect("legacy EPUB writes");
        let outcome = write_chaptered_epub(
            &blocks,
            "Book",
            "en",
            &unmatched,
            Some("Chapter "),
            "test-source",
            DETERMINISTIC_MODIFIED,
        )
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

        write_epub(
            &blocks,
            "Book",
            "en",
            &legacy,
            "test-source",
            DETERMINISTIC_MODIFIED,
        )
        .expect("legacy EPUB writes");
        let outcome = write_chaptered_epub(
            &blocks,
            "Book",
            "en",
            &guarded,
            Some(""),
            "test-source",
            DETERMINISTIC_MODIFIED,
        )
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

    #[test]
    fn level_one_headings_split_chapters_and_merge_a_half_title_into_the_title_page() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("headings.epub");
        let blocks = vec![
            heading(1, "Synthetic Book"),
            heading(1, "Synthetic Book"),
            paragraph("A. Writer"),
            heading(1, "One"),
            paragraph("First body."),
            heading(2, "Section A"),
            paragraph("Section body."),
            heading(1, "Two"),
            paragraph("Second body."),
            heading(3, "Detail"),
            paragraph("Detail body."),
        ];

        let outcome = write_chaptered_epub(
            &blocks,
            "Synthetic Book",
            "en",
            &path,
            None,
            "test-source",
            DETERMINISTIC_MODIFIED,
        )
        .expect("split EPUB writes");

        assert_eq!(
            outcome,
            ChapterSplitOutcome::Split {
                blocks_per_chapter: vec![3, 4, 4]
            }
        );
        let names = zip_names(&path);
        assert!(!names.iter().any(|name| name == "content.xhtml"));
        let chapters = names
            .iter()
            .filter(|name| name.starts_with("chapter-"))
            .collect::<Vec<_>>();
        assert_eq!(chapters.len(), 3, "{names:?}");
        for chapter in chapters {
            assert!(
                zip_entry(&path, chapter).contains("<p>"),
                "{chapter} must not be heading-only"
            );
        }

        // The half-title and title page share one file, ahead of the body.
        let title_page = zip_entry(&path, "chapter-0001.xhtml");
        assert!(title_page.contains("<h1 id=\"head-0000\">Synthetic Book</h1>"));
        assert!(title_page.contains("<h1 id=\"head-0001\">Synthetic Book</h1>"));
        assert!(title_page.contains("A. Writer"));
        assert!(!title_page.contains("One"));

        // Heading ids keep counting across files.
        let second = zip_entry(&path, "chapter-0003.xhtml");
        assert!(second.contains("<h1 id=\"head-0004\">Two</h1>"), "{second}");
        assert!(
            second.contains("<h3 id=\"head-0005\">Detail</h3>"),
            "{second}"
        );

        let nav = zip_entry(&path, "nav.xhtml");
        assert!(
            nav.contains("<li><a href=\"chapter-0001.xhtml\">Synthetic Book</a></li>"),
            "a repeated title is not listed under itself: {nav}"
        );
        assert!(nav.contains(
            "<li><a href=\"chapter-0002.xhtml\">One</a><ol><li><a href=\"chapter-0002.xhtml#head-0003\">Section A</a></li></ol></li>"
        ));
        assert!(nav.contains(
            "<li><a href=\"chapter-0003.xhtml\">Two</a><ol><li><a href=\"chapter-0003.xhtml#head-0005\">Detail</a></li></ol></li>"
        ));
        assert_nav_links_resolve(&path);

        let book = bookforge_epub::read_epub(&path).expect("split EPUB reads");
        assert_eq!(book.spine.len(), 3);
        assert_eq!(
            bookforge_epub::text_coverage(&path).unwrap().percent(),
            100.0
        );
    }

    #[test]
    fn nav_links_resolve_across_files_with_front_matter_part_title_and_tail() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("structure.epub");
        let blocks = vec![
            paragraph("Front matter note."),
            heading(3, "Epigraph"),
            paragraph("Quoted line."),
            heading(1, "Part One"),
            heading(1, "Chapter 1"),
            paragraph("First body."),
            heading(2, "A Section"),
            paragraph("Section body."),
            heading(1, "Chapter 2"),
            paragraph("Second body."),
            heading(1, "The End"),
        ];

        let outcome = write_chaptered_epub(
            &blocks,
            "Synthetic Book",
            "en",
            &path,
            None,
            "test-source",
            DETERMINISTIC_MODIFIED,
        )
        .expect("split EPUB writes");

        // Front matter with body text keeps its own file; the part title
        // opens the chapter after it; the heading-only tail closes the
        // chapter before it.
        assert_eq!(
            outcome,
            ChapterSplitOutcome::Split {
                blocks_per_chapter: vec![3, 5, 3]
            }
        );
        let nav = zip_entry(&path, "nav.xhtml");
        assert!(nav.contains(
            "<li><a href=\"chapter-0001.xhtml\">Synthetic Book</a><ol><li><a href=\"chapter-0001.xhtml#head-0000\">Epigraph</a></li></ol></li>"
        ), "{nav}");
        assert!(nav.contains(
            "<li><a href=\"chapter-0002.xhtml\">Part One</a><ol><li><a href=\"chapter-0002.xhtml#head-0002\">Chapter 1</a><ol><li><a href=\"chapter-0002.xhtml#head-0003\">A Section</a></li></ol></li></ol></li>"
        ), "{nav}");
        assert!(nav.contains(
            "<li><a href=\"chapter-0003.xhtml\">Chapter 2</a><ol><li><a href=\"chapter-0003.xhtml#head-0005\">The End</a></li></ol></li>"
        ), "{nav}");
        let links = assert_nav_links_resolve(&path);
        assert_eq!(links.len(), 7, "{links:?}");
    }

    #[test]
    fn heading_only_front_matter_opens_the_first_chapter() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("front.epub");
        let blocks = vec![
            heading(2, "Series Name"),
            heading(1, "Chapter 1"),
            paragraph("First body."),
            heading(1, "Chapter 2"),
            paragraph("Second body."),
        ];

        let outcome = write_chaptered_epub(
            &blocks,
            "Synthetic Book",
            "en",
            &path,
            None,
            "test-source",
            DETERMINISTIC_MODIFIED,
        )
        .expect("split EPUB writes");

        assert_eq!(
            outcome,
            ChapterSplitOutcome::Split {
                blocks_per_chapter: vec![3, 2]
            }
        );
        let first = zip_entry(&path, "chapter-0001.xhtml");
        assert!(first.contains("<title>Chapter 1</title>"), "{first}");
        assert!(first.contains("Series Name"));
        assert_nav_links_resolve(&path);
    }

    #[test]
    fn a_single_level_one_heading_keeps_the_single_file_writer() {
        let dir = tempfile::tempdir().expect("temp dir");
        let legacy = dir.path().join("legacy.epub");
        let split = dir.path().join("split.epub");
        // Front matter ahead of the only level-1 heading does not make a
        // second chapter: a paper's title is not a chapter boundary.
        let blocks = vec![
            paragraph("Journal of Synthetic Fixtures, vol. 1"),
            heading(1, "Paper Title"),
            paragraph("Abstract text."),
            heading(2, "Methods"),
            paragraph("Methods text."),
        ];

        write_epub(
            &blocks,
            "Paper Title",
            "en",
            &legacy,
            "test-source",
            DETERMINISTIC_MODIFIED,
        )
        .expect("legacy EPUB writes");
        let outcome = write_chaptered_epub(
            &blocks,
            "Paper Title",
            "en",
            &split,
            None,
            "test-source",
            DETERMINISTIC_MODIFIED,
        )
        .expect("EPUB writes");

        assert_eq!(outcome, ChapterSplitOutcome::SingleChapter);
        assert_eq!(
            std::fs::read(legacy).expect("legacy EPUB reads"),
            std::fs::read(split).expect("EPUB reads")
        );
    }

    #[test]
    fn heading_only_chapters_merging_down_to_one_keep_the_single_file_writer() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("merged.epub");
        let blocks = vec![
            heading(1, "Half Title"),
            heading(1, "Title"),
            paragraph("Body one."),
            paragraph("Body two."),
            paragraph("Body three."),
            heading(1, "Colophon"),
        ];

        let outcome = write_chaptered_epub(
            &blocks,
            "Title",
            "en",
            &path,
            None,
            "test-source",
            DETERMINISTIC_MODIFIED,
        )
        .expect("EPUB writes");

        assert_eq!(outcome, ChapterSplitOutcome::SingleChapter);
        assert!(zip_names(&path).iter().any(|name| name == "content.xhtml"));
    }

    #[test]
    fn crowded_level_one_headings_are_guarded_by_the_legacy_epub() {
        let dir = tempfile::tempdir().expect("temp dir");
        let legacy = dir.path().join("legacy.epub");
        let guarded = dir.path().join("guarded.epub");
        // Three level-1 headings among five text blocks is fewer than two
        // text blocks per chapter: heading detection has gone wrong.
        let blocks = vec![
            heading(1, "Alpha"),
            paragraph("One."),
            heading(1, "Beta"),
            paragraph("Two."),
            heading(1, "Gamma"),
        ];

        write_epub(
            &blocks,
            "Book",
            "en",
            &legacy,
            "test-source",
            DETERMINISTIC_MODIFIED,
        )
        .expect("legacy EPUB writes");
        let outcome = write_chaptered_epub(
            &blocks,
            "Book",
            "en",
            &guarded,
            None,
            "test-source",
            DETERMINISTIC_MODIFIED,
        )
        .expect("guarded EPUB writes");

        assert_eq!(
            outcome,
            ChapterSplitOutcome::Guarded {
                matches: 3,
                text_blocks: 5
            }
        );
        assert_eq!(
            std::fs::read(legacy).expect("legacy EPUB reads"),
            std::fs::read(guarded).expect("guarded EPUB reads")
        );
    }

    #[test]
    fn too_many_level_one_headings_are_guarded_by_the_legacy_epub() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("many.epub");
        let blocks = (0..=MAX_CHAPTER_MATCHES)
            .flat_map(|index| {
                [
                    heading(1, &format!("Heading {index}")),
                    paragraph("Body one."),
                    paragraph("Body two."),
                ]
            })
            .collect::<Vec<_>>();

        let outcome = write_chaptered_epub(
            &blocks,
            "Book",
            "en",
            &path,
            None,
            "test-source",
            DETERMINISTIC_MODIFIED,
        )
        .expect("guarded EPUB writes");

        assert_eq!(
            outcome,
            ChapterSplitOutcome::Guarded {
                matches: MAX_CHAPTER_MATCHES + 1,
                text_blocks: (MAX_CHAPTER_MATCHES + 1) * 3
            }
        );
        assert!(zip_names(&path).iter().any(|name| name == "content.xhtml"));
    }
}
