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
    max_request_bytes: usize,
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
        if image.len() > max_request_bytes {
            outcome.warnings.push(format!(
                "page {page_number}: OCR skipped because the rendered PNG ({} bytes) exceeds the {max_request_bytes}-byte request limit",
                image.len()
            ));
            continue;
        }
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
    let mut kept = Vec::with_capacity(old_blocks.len());
    let mut insert_at = None;

    for anchored in old_blocks {
        // OCR replaces the garbage *text* of the page only. Figure/media
        // blocks stay anchored exactly where they were (PDF-5): their
        // raster crops and captions remain valid regardless of the text
        // layer's quality.
        if anchored.anchor.page == page_number && !matches!(anchored.block, DocBlock::Figure { .. })
        {
            insert_at.get_or_insert(kept.len());
            continue;
        }
        if insert_at.is_none() && anchored.anchor.page > page_number {
            insert_at.get_or_insert(kept.len());
        }
        kept.push(anchored);
    }

    let insert_at = insert_at.unwrap_or(kept.len());
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

    kept.splice(
        insert_at..insert_at,
        paragraphs
            .into_iter()
            .enumerate()
            .map(|(offset, paragraph)| AnchoredBlock {
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
            }),
    );
    *blocks = kept;
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

    let page_dir = ScopedTempDir::new("bookforge-pdf-pages")?;
    let mut warnings = Vec::new();
    let result = (|| {
        for page_number in low_confidence_pages {
            let rendered = match tools.render_page_png(input, *page_number, page_dir.path()) {
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
    drop(page_dir);
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
    // extract_images always invokes `pdfimages -png`, so emitted files
    // are PNGs; unknown extensions keep the PNG identity rather than
    // advertising a JPEG we never produce.
    const MEDIA_TYPE: &str = "image/png";
    let id = format!("pdf-image-{:04}", image.index + 1);
    Ok(ImageAsset {
        id,
        href: format!("images/pdf-image-{:04}.png", image.index + 1),
        media_type: MEDIA_TYPE.to_string(),
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

    fn figure(page: u32, top: i32) -> AnchoredBlock {
        AnchoredBlock {
            block: DocBlock::Figure {
                image: ImageAsset {
                    id: format!("pdf-figure-{page:04}"),
                    href: "images/figure.png".to_string(),
                    media_type: "image/png".to_string(),
                    bytes: Vec::new(),
                    page,
                    top: Some(top),
                    left: Some(0),
                    width: Some(1),
                    height: Some(1),
                },
                caption: Some(vec![Span {
                    text: "Figure 1. Captured raster.".to_string(),
                    bold: false,
                    italic: false,
                }]),
            },
            anchor: BlockAnchor {
                page,
                top,
                left: 0,
                width: 1,
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
            usize::MAX,
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
        let outcome = ocr_low_confidence_pages(
            &engine,
            &mut |_page| Ok(vec![1, 2, 3]),
            &mut blocks,
            &[3],
            usize::MAX,
        );

        assert!(outcome.recovered.is_empty());
        assert_eq!(blocks[0].block.text(), "original");
        assert!(outcome.warnings[0].contains("page 3: OCR failed"));
        assert!(outcome.warnings[0].contains("offline"));
    }

    #[test]
    fn recovered_pages_are_excluded_from_later_preservation() {
        let engine = MockOcrEngine::success("Recovered");
        let mut blocks = vec![paragraph(1, "bad one"), paragraph(2, "bad two")];
        let outcome = ocr_low_confidence_pages(
            &engine,
            &mut |_page| Ok(vec![1, 2, 3]),
            &mut blocks,
            &[1],
            usize::MAX,
        );
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

    #[test]
    fn successful_ocr_preserves_figure_blocks_anchored_on_the_page() {
        // PDF-5: a successful OCR pass must not wipe figure/media anchors
        // that share the replaced page.
        let engine = MockOcrEngine::success("Recovered prose from scan.");
        let mut blocks = vec![
            paragraph(1, "garbage text layer"),
            figure(1, 120),
            paragraph(2, "keep"),
        ];
        let outcome = ocr_low_confidence_pages(
            &engine,
            &mut |_page| Ok(vec![1, 2, 3]),
            &mut blocks,
            &[1],
            usize::MAX,
        );

        assert_eq!(outcome.recovered, vec![1]);
        assert!(outcome.warnings.is_empty());
        let texts = blocks
            .iter()
            .map(|block| block.block.text())
            .collect::<Vec<_>>();
        assert!(
            texts.contains(&"Recovered prose from scan.".to_string()),
            "OCR paragraphs must be inserted: {texts:?}"
        );
        let figures = blocks
            .iter()
            .filter(|block| matches!(block.block, DocBlock::Figure { .. }))
            .count();
        assert_eq!(figures, 1, "figure anchor must survive OCR: {blocks:?}");
        assert_eq!(blocks.last().expect("blocks").block.text(), "keep");
        let figure_index = blocks
            .iter()
            .position(|block| matches!(block.block, DocBlock::Figure { .. }))
            .expect("figure present");
        let ocr_index = blocks
            .iter()
            .position(|block| block.block.text() == "Recovered prose from scan.")
            .expect("ocr paragraph present");
        assert!(
            ocr_index < figure_index,
            "OCR text belongs before the retained lower figure anchor"
        );
    }

    #[derive(Debug, Default)]
    struct CountingEngine {
        calls: std::sync::atomic::AtomicUsize,
    }

    impl OcrEngine for CountingEngine {
        fn ocr_page(
            &self,
            _image_png: &[u8],
            _page_number: u32,
        ) -> std::result::Result<String, crate::ocr::OcrError> {
            use std::sync::atomic::Ordering;
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok("should not happen".to_string())
        }
    }

    #[test]
    fn oversized_raster_is_skipped_with_a_warning_before_encoding() {
        // PDF-22: extreme rasters must not reach base64 encoding / the
        // engine; they are skipped and warned about instead.
        let engine = CountingEngine::default();
        let mut blocks = vec![paragraph(1, "garbage")];
        let big_image = vec![0_u8; 4096];
        let outcome = ocr_low_confidence_pages(
            &engine,
            &mut |_page| Ok(big_image.clone()),
            &mut blocks,
            &[1],
            1024,
        );

        assert!(outcome.recovered.is_empty());
        assert_eq!(engine.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(outcome.warnings.len(), 1);
        assert!(outcome.warnings[0].contains("exceeds the 1024-byte request limit"));
        assert_eq!(blocks[0].block.text(), "garbage", "page stays as-is");
    }
}
