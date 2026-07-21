use super::*;

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct OcrPagesOutcome {
    pub recovered: Vec<u32>,
    pub warnings: Vec<String>,
}

pub(super) fn ocr_low_confidence_pages(
    engine: &dyn OcrEngine,
    render: &mut dyn FnMut(u32) -> Result<Vec<u8>>,
    blocks: &mut Vec<AnchoredBlock>,
    low_confidence_pages: &[u32],
) -> OcrPagesOutcome {
    let mut outcome = OcrPagesOutcome::default();
    for page_number in low_confidence_pages {
        let image = match render(*page_number) {
            Ok(image) => image,
            Err(error) => {
                outcome.warnings.push(format!(
                    "page {page_number}: OCR skipped because raster rendering failed: {error}"
                ));
                continue;
            }
        };
        match engine.ocr_page(&image, *page_number) {
            Ok(text) => {
                replace_page_with_ocr_text(blocks, *page_number, &text);
                outcome.recovered.push(*page_number);
            }
            Err(error) => outcome
                .warnings
                .push(format!("page {page_number}: OCR failed: {error}")),
        }
    }
    outcome
}

fn replace_page_with_ocr_text(blocks: &mut Vec<AnchoredBlock>, page_number: u32, text: &str) {
    let old_blocks = std::mem::take(blocks);
    let mut insert_at = None;

    for anchored in old_blocks {
        if anchored.anchor.page == page_number {
            insert_at.get_or_insert(blocks.len());
            continue;
        }
        if anchored.anchor.page > page_number {
            insert_at.get_or_insert(blocks.len());
        }
        blocks.push(anchored);
    }

    let insert_at = insert_at.unwrap_or(blocks.len());
    let mut paragraphs = Vec::new();
    let mut lines = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            if !lines.is_empty() {
                paragraphs.push(lines.join("\n"));
                lines.clear();
            }
        } else {
            lines.push(line);
        }
    }
    if !lines.is_empty() {
        paragraphs.push(lines.join("\n"));
    }

    for (offset, paragraph) in paragraphs.into_iter().enumerate() {
        blocks.insert(
            insert_at + offset,
            AnchoredBlock {
                block: DocBlock::Paragraph {
                    spans: vec![Span {
                        text: paragraph,
                        bold: false,
                        italic: false,
                    }],
                },
                anchor: BlockAnchor {
                    page: page_number,
                    top: i32::try_from(offset).unwrap_or(i32::MAX),
                    left: 0,
                    width: 1,
                },
            },
        );
    }
}

pub(super) fn preserve_low_confidence_pages(
    input: &Path,
    pages: &[Page],
    tools: &PopplerTools,
    blocks: &mut Vec<AnchoredBlock>,
    low_confidence_pages: &[u32],
) -> Result<Vec<String>> {
    if low_confidence_pages.is_empty() {
        return Ok(Vec::new());
    }

    let page_dir = scoped_temp_dir("bookforge-pdf-pages")?;
    let mut warnings = Vec::new();
    let result = (|| {
        for page_number in low_confidence_pages {
            let rendered = match tools.render_page_png(input, *page_number, &page_dir) {
                Ok(rendered) => rendered,
                Err(err) => {
                    warnings.push(format!(
                        "page {page_number}: low-confidence page image preservation skipped because raster rendering failed: {err}"
                    ));
                    continue;
                }
            };
            let source_page = pages.iter().find(|page| page.number == *page_number);
            let asset = page_image_asset(*page_number, source_page, &rendered)?;
            replace_page_with_preserved_image(blocks, *page_number, asset);
        }
        Ok(warnings)
    })();
    let _ = fs::remove_dir_all(&page_dir);
    result
}

fn page_image_asset(page_number: u32, page: Option<&Page>, path: &Path) -> Result<ImageAsset> {
    let bytes = fs::read(path)?;
    Ok(ImageAsset {
        id: format!("pdf-page-{page_number:04}"),
        href: format!("images/pdf-page-{page_number:04}.png"),
        media_type: "image/png".to_string(),
        bytes,
        page: page_number,
        top: Some(0),
        left: Some(0),
        width: page.map(|page| page.width),
        height: page.map(|page| page.height),
    })
}

fn replace_page_with_preserved_image(
    blocks: &mut Vec<AnchoredBlock>,
    page_number: u32,
    image: ImageAsset,
) {
    let old_blocks = std::mem::take(blocks);
    let mut insert_at = None;

    for anchored in old_blocks {
        if anchored.anchor.page == page_number {
            insert_at.get_or_insert(blocks.len());
            continue;
        }
        if anchored.anchor.page > page_number {
            insert_at.get_or_insert(blocks.len());
        }
        blocks.push(anchored);
    }

    let insert_at = insert_at.unwrap_or(blocks.len());
    let width = image.width.unwrap_or(1);
    blocks.insert(
        insert_at,
        AnchoredBlock {
            block: DocBlock::Figure {
                image,
                caption: None,
            },
            anchor: BlockAnchor {
                page: page_number,
                top: 0,
                left: 0,
                width,
            },
        },
    );
}

pub(super) struct PageCropRenderer<'a> {
    input: &'a Path,
    tools: &'a PopplerTools,
    page_dir: &'a Path,
    rendered_pages: HashMap<u32, PathBuf>,
}

impl<'a> PageCropRenderer<'a> {
    pub(super) fn new(input: &'a Path, tools: &'a PopplerTools, page_dir: &'a Path) -> Self {
        Self {
            input,
            tools,
            page_dir,
            rendered_pages: HashMap::new(),
        }
    }

    fn render_page(&mut self, page: u32) -> Result<PathBuf> {
        if let Some(path) = self.rendered_pages.get(&page) {
            return Ok(path.clone());
        }
        let path = self
            .tools
            .render_page_png(self.input, page, self.page_dir)?;
        self.rendered_pages.insert(page, path.clone());
        Ok(path)
    }

    pub(super) fn render_crop(
        &mut self,
        crop: PageCrop,
        output_dir: &Path,
        name: &str,
    ) -> Result<PathBuf> {
        fs::create_dir_all(output_dir)?;
        let full_page = self.render_page(crop.page)?;
        let output = output_dir.join(format!("{name}.png"));
        crop_png_to_file(&full_page, crop.to_render_pixels(), &output)?;
        Ok(output)
    }
}

pub(super) fn media_asset(
    kind: MediaKind,
    index: usize,
    rect: RegionRect,
    path: &Path,
) -> Result<ImageAsset> {
    let bytes = fs::read(path)?;
    let prefix = kind.id_prefix();
    Ok(ImageAsset {
        id: format!("{prefix}-{index:04}"),
        href: format!("images/{prefix}-{index:04}.png"),
        media_type: "image/png".to_string(),
        bytes,
        page: rect.page,
        top: Some(rect.top),
        left: Some(rect.left),
        width: Some(rect.width),
        height: Some(rect.height),
    })
}

pub(super) fn render_figure_crop(
    crop_renderer: &mut PageCropRenderer<'_>,
    output_dir: &Path,
    index: usize,
    rect: RegionRect,
    prefix: &str,
) -> Result<ImageAsset> {
    let name = format!("{prefix}-{index:04}");
    let rendered = crop_renderer.render_crop(
        PageCrop {
            page: rect.page,
            left: rect.left,
            top: rect.top,
            width: rect.width,
            height: rect.height,
        },
        output_dir,
        &name,
    )?;
    figure_crop_asset(index, rect, &rendered)
}

fn figure_crop_asset(index: usize, rect: RegionRect, path: &Path) -> Result<ImageAsset> {
    let bytes = fs::read(path)?;
    Ok(ImageAsset {
        id: format!("pdf-figure-{index:04}"),
        href: format!("images/pdf-figure-{index:04}.png"),
        media_type: "image/png".to_string(),
        bytes,
        page: rect.page,
        top: Some(rect.top),
        left: Some(rect.left),
        width: Some(rect.width),
        height: Some(rect.height),
    })
}

pub(super) fn image_asset(
    image: &ExtractedImage,
    region: Option<&ImageRegion>,
) -> Result<ImageAsset> {
    let bytes = fs::read(&image.path)?;
    let extension = match image.extension.as_str() {
        "jpg" | "jpeg" => "jpg",
        _ => "png",
    };
    let media_type = match extension {
        "jpg" => "image/jpeg",
        _ => "image/png",
    };
    let id = format!("pdf-image-{:04}", image.index + 1);
    Ok(ImageAsset {
        id,
        href: format!("images/pdf-image-{:04}.{extension}", image.index + 1),
        media_type: media_type.to_string(),
        bytes,
        page: image.page,
        top: region.map(|region| region.top),
        left: region.map(|region| region.left),
        width: region
            .map(|region| region.width)
            .or_else(|| image.width.map(|width| width as i32)),
        height: region
            .map(|region| region.height)
            .or_else(|| image.height.map(|height| height as i32)),
    })
}

pub(super) fn insert_figure_blocks(
    blocks: &mut Vec<AnchoredBlock>,
    mut figures: Vec<AnchoredFigure>,
    warnings: &mut Vec<String>,
) -> usize {
    figures.sort_by_key(|figure| (figure.anchor.page, figure.anchor.top));
    let mut removed_chars = 0;

    for figure in figures {
        if let Some(region) = figure.text_region {
            removed_chars += remove_blocks_in_region(blocks, region, warnings);
        }
        if let DocBlock::Figure {
            caption: Some(caption),
            ..
        } = &figure.block
        {
            remove_duplicate_caption_block(blocks, figure.anchor.page, &spans_text(caption));
        }

        let insert_at = blocks
            .iter()
            .position(|anchored| {
                anchored.anchor.page > figure.anchor.page
                    || anchored.anchor.page == figure.anchor.page
                        && anchored.anchor.top > figure.anchor.top
            })
            .unwrap_or(blocks.len());
        blocks.insert(
            insert_at,
            AnchoredBlock {
                block: figure.block,
                anchor: figure.anchor,
            },
        );
    }

    removed_chars
}

pub(super) fn remove_blocks_in_region(
    blocks: &mut Vec<AnchoredBlock>,
    region: RegionRect,
    warnings: &mut Vec<String>,
) -> usize {
    let mut index = 0;
    let mut removed_chars = 0;
    while index < blocks.len() {
        let anchor = blocks[index].anchor;
        if anchor.page == region.page
            && anchor.top >= region.top
            && anchor.top < region.bottom()
            && anchor_overlaps_region_horizontally(anchor, region)
        {
            let text = blocks[index].block.text();
            if is_prose_like_text(&text) {
                warnings.push(format!(
                    "page {}: kept overlapping prose as text near image region y={} ({} chars): {}",
                    region.page,
                    region.top,
                    blocks[index].block.char_count(),
                    audit_snippet(&text)
                ));
                index += 1;
                continue;
            }
            let removed = blocks.remove(index);
            let chars = removed.block.char_count();
            removed_chars += chars;
            if !text.trim().is_empty() {
                warnings.push(format!(
                    "page {}: preserved as image near y={} ({} chars): {}",
                    region.page,
                    region.top,
                    chars,
                    audit_snippet(&text)
                ));
            }
        } else {
            index += 1;
        }
    }
    removed_chars
}

fn is_prose_like_text(text: &str) -> bool {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    normalized.chars().count() >= 60
        && normalized
            .chars()
            .any(|ch| matches!(ch, '.' | '!' | '?' | ';' | ':'))
        && normalized.split_whitespace().count() > 8
        && normalized.chars().any(|ch| ch.is_alphabetic())
}

fn remove_duplicate_caption_block(blocks: &mut Vec<AnchoredBlock>, page: u32, caption: &str) {
    let normalized = normalize_text_key(caption);
    let Some(index) = blocks.iter().position(|anchored| {
        anchored.anchor.page == page
            && matches!(
                &anchored.block,
                DocBlock::Paragraph { .. } | DocBlock::Heading { .. }
            )
            && normalize_text_key(&anchored.block.text()) == normalized
    }) else {
        return;
    };

    blocks.remove(index);
}

fn anchor_overlaps_region_horizontally(anchor: BlockAnchor, region: RegionRect) -> bool {
    anchor.left < region.right() && anchor.left + anchor.width.max(1) > region.left
}

fn audit_snippet(text: &str) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    const LIMIT: usize = 140;
    if normalized.chars().count() <= LIMIT {
        return normalized;
    }
    let mut snippet = normalized.chars().take(LIMIT).collect::<String>();
    snippet.push_str("...");
    snippet
}

#[cfg(test)]
mod ocr_tests {
    use crate::ocr::MockOcrEngine;

    use super::*;

    fn paragraph(page: u32, text: &str) -> AnchoredBlock {
        AnchoredBlock {
            block: DocBlock::Paragraph {
                spans: vec![Span {
                    text: text.to_string(),
                    bold: false,
                    italic: false,
                }],
            },
            anchor: BlockAnchor {
                page,
                top: 10,
                left: 10,
                width: 100,
            },
        }
    }

    #[test]
    fn successful_ocr_replaces_page_with_paragraphs() {
        let engine = MockOcrEngine::success("First OCR paragraph.\n\nSecond OCR paragraph.");
        let mut blocks = vec![paragraph(1, "garbage"), paragraph(2, "keep")];
        let mut rendered = Vec::new();
        let outcome = ocr_low_confidence_pages(
            &engine,
            &mut |page| {
                rendered.push(page);
                Ok(vec![1, 2, 3])
            },
            &mut blocks,
            &[1],
        );

        assert_eq!(rendered, vec![1]);
        assert_eq!(outcome.recovered, vec![1]);
        assert!(outcome.warnings.is_empty());
        assert_eq!(
            blocks
                .iter()
                .map(|block| block.block.text())
                .collect::<Vec<_>>(),
            vec!["First OCR paragraph.", "Second OCR paragraph.", "keep"]
        );
        assert_eq!(blocks[0].anchor.top, 0);
        assert_eq!(blocks[1].anchor.top, 1);
    }

    #[test]
    fn failed_ocr_leaves_page_blocks_intact_and_warns() {
        let engine = MockOcrEngine::failure("offline");
        let mut blocks = vec![paragraph(3, "original")];
        let outcome =
            ocr_low_confidence_pages(&engine, &mut |_page| Ok(vec![1, 2, 3]), &mut blocks, &[3]);

        assert!(outcome.recovered.is_empty());
        assert_eq!(blocks[0].block.text(), "original");
        assert!(outcome.warnings[0].contains("page 3: OCR failed"));
        assert!(outcome.warnings[0].contains("offline"));
    }

    #[test]
    fn recovered_pages_are_excluded_from_later_preservation() {
        let engine = MockOcrEngine::success("Recovered");
        let mut blocks = vec![paragraph(1, "bad one"), paragraph(2, "bad two")];
        let outcome =
            ocr_low_confidence_pages(&engine, &mut |_page| Ok(vec![1, 2, 3]), &mut blocks, &[1]);
        let mut preserve_pages = vec![1, 2];
        preserve_pages.retain(|page| !outcome.recovered.contains(page));

        for page in preserve_pages {
            replace_page_with_preserved_image(
                &mut blocks,
                page,
                ImageAsset {
                    id: "preserved".to_string(),
                    href: "images/preserved.png".to_string(),
                    media_type: "image/png".to_string(),
                    bytes: vec![1],
                    page,
                    top: Some(0),
                    left: Some(0),
                    width: Some(1),
                    height: Some(1),
                },
            );
        }

        assert!(matches!(blocks[0].block, DocBlock::Paragraph { .. }));
        assert_eq!(blocks[0].block.text(), "Recovered");
        assert!(matches!(blocks[1].block, DocBlock::Figure { .. }));
    }
}
