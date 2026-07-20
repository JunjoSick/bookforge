use super::*;

#[cfg(unix)]
const BERT_FIGURE1_CAPTION_BOUNDARY_XML: &str =
    include_str!("../../fixtures/bert_figure1_caption_boundary.xml");
#[cfg(unix)]
const BERT_FIGURE4_MULTIPANEL_XML: &str =
    include_str!("../../fixtures/bert_figure4_multipanel.xml");
#[cfg(unix)]
const BERT_FIGURE5_VECTOR_CHART_XML: &str =
    include_str!("../../fixtures/bert_figure5_vector_chart.xml");
const BERT_PAGE16_VECTOR_CHART_TWOCOL_XML: &str =
    include_str!("../../fixtures/bert_page16_vector_chart_twocol.xml");
const BERT_FIGURE1_TOKEN_STRIP_XML: &str =
    include_str!("../../fixtures/bert_figure1_token_strip.xml");
#[cfg(unix)]
const BERT_MODEL_PARAMETER_FALSE_POSITIVE_XML: &str =
    include_str!("../../fixtures/bert_model_parameter_false_positive.xml");

fn span(text: &str) -> Span {
    Span {
        text: text.to_string(),
        bold: false,
        italic: false,
    }
}

fn fragment(top: i32, left: i32, width: i32, height: i32, text: &str) -> Fragment {
    Fragment {
        top,
        left,
        width,
        height,
        font: 0,
        spans: vec![span(text)],
    }
}

fn empty_page(number: u32) -> Page {
    Page {
        number,
        width: 600,
        height: 800,
        fragments: Vec::new(),
        images: Vec::new(),
        font_sizes: HashMap::new(),
    }
}

#[cfg(unix)]
fn write_executable(path: &Path, script: &str) {
    use std::{fs, io::Write, os::unix::fs::PermissionsExt};

    let mut file = fs::File::create(path).expect("tool fixture");
    file.write_all(script.as_bytes()).expect("tool fixture");
    file.sync_all().expect("tool fixture sync");
    drop(file);
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("tool executable");
}

#[cfg(unix)]
fn fake_pdftohtml_with_xml(path: &Path, xml: &str) {
    write_executable(
        path,
        &format!(
            r#"#!/bin/sh
cat <<'XML'
{xml}
XML
"#
        ),
    );
}

#[cfg(unix)]
fn fake_pdftotext_with_text(path: &Path, text: &str) {
    write_executable(
        path,
        &format!(
            r#"#!/bin/sh
cat <<'TEXT'
{text}
TEXT
"#
        ),
    );
}

#[cfg(unix)]
fn fake_pdfimages(path: &Path) {
    write_executable(
        path,
        r#"#!/bin/sh
if [ "$1" = "-list" ]; then
cat <<'LIST'
page   num  type   width height color comp bpc  enc interp  object ID x-ppi y-ppi size ratio
--------------------------------------------------------------------------------------------
   1     0 image     120    80  rgb     3   8  image  no        12  0    72    72  1K  1.0%
LIST
exit 0
fi
for last do :; done
printf 'fake-image' > "$last-000-000.png"
echo "$last-000-000.png"
"#,
    );
}

#[cfg(unix)]
fn fake_pdfimages_two(path: &Path) {
    write_executable(
        path,
        r#"#!/bin/sh
if [ "$1" = "-list" ]; then
cat <<'LIST'
page   num  type   width height color comp bpc  enc interp  object ID x-ppi y-ppi size ratio
--------------------------------------------------------------------------------------------
   1     0 image     180    90  rgb     3   8  image  no        12  0    72    72  1K  1.0%
   1     1 image     180    90  rgb     3   8  image  no        13  0    72    72  1K  1.0%
LIST
exit 0
fi
for last do :; done
printf 'fake-panel-a' > "$last-000-000.png"
printf 'fake-panel-b' > "$last-000-001.png"
echo "$last-000-000.png"
echo "$last-000-001.png"
"#,
    );
}

#[cfg(unix)]
fn fake_pdfimages_empty(path: &Path) {
    write_executable(
        path,
        r#"#!/bin/sh
if [ "$1" = "-list" ]; then
cat <<'LIST'
page   num  type   width height color comp bpc  enc interp  object ID x-ppi y-ppi size ratio
--------------------------------------------------------------------------------------------
LIST
fi
"#,
    );
}

#[cfg(unix)]
fn fake_pdftoppm(path: &Path) {
    let fixture = path.with_file_name("pdftoppm.fixture.png");
    crate::tools::write_solid_rgb_png(&fixture, 1600, 2200, [240, 240, 240])
        .expect("pdftoppm PNG fixture");
    write_executable(
        path,
        r#"#!/bin/sh
for last do :; done
script_dir=$(dirname "$0")
cp "$script_dir/pdftoppm.fixture.png" "$last.png"
"#,
    );
}

#[test]
fn remove_blocks_in_region_requires_horizontal_overlap() {
    let mut blocks = vec![
        AnchoredBlock {
            block: DocBlock::Paragraph {
                spans: vec![span("left table text 2019 2020")],
            },
            anchor: BlockAnchor {
                page: 1,
                top: 120,
                left: 100,
                width: 180,
            },
        },
        AnchoredBlock {
            block: DocBlock::Paragraph {
                spans: vec![span("right column prose must remain")],
            },
            anchor: BlockAnchor {
                page: 1,
                top: 120,
                left: 420,
                width: 160,
            },
        },
    ];
    let mut warnings = Vec::new();

    let removed = remove_blocks_in_region(
        &mut blocks,
        RegionRect {
            page: 1,
            top: 100,
            left: 90,
            width: 220,
            height: 80,
        },
        &mut warnings,
    );

    assert!(removed > 0);
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].block.text(), "right column prose must remain");
    assert!(warnings[0].contains("left table text"));
}

#[test]
fn remove_blocks_in_region_keeps_prose_like_blocks_as_text() {
    let mut blocks = vec![AnchoredBlock {
        block: DocBlock::Paragraph {
            spans: vec![span(
                "This prose sentence should remain translatable even when a table crop overlaps its anchor and geometry.",
            )],
        },
        anchor: BlockAnchor {
            page: 1,
            top: 120,
            left: 100,
            width: 320,
        },
    }];
    let mut warnings = Vec::new();

    let removed = remove_blocks_in_region(
        &mut blocks,
        RegionRect {
            page: 1,
            top: 100,
            left: 90,
            width: 340,
            height: 80,
        },
        &mut warnings,
    );

    assert_eq!(removed, 0);
    assert_eq!(blocks.len(), 1);
    assert!(warnings[0].contains("kept overlapping prose as text"));
    assert!(!warnings[0].contains("preserved as image"));
}

#[test]
fn image_regions_match_by_dimensions_not_position() {
    let page = Page {
        number: 1,
        width: 600,
        height: 800,
        fragments: vec![
            fragment(210, 100, 240, 12, "Figure 1. Wide image."),
            fragment(575, 100, 240, 12, "Figure 2. Tall image."),
        ],
        images: vec![
            ImageRegion {
                top: 100,
                left: 100,
                width: 270,
                height: 90,
                src: None,
            },
            ImageRegion {
                top: 300,
                left: 100,
                width: 90,
                height: 270,
                src: None,
            },
        ],
        font_sizes: HashMap::new(),
    };
    let pages_by_number = HashMap::from([(1, &page)]);
    let images = vec![
        ExtractedImage {
            page: 1,
            index: 0,
            width: Some(100),
            height: Some(300),
            path: PathBuf::from("tall.png"),
            extension: "png".to_string(),
        },
        ExtractedImage {
            page: 1,
            index: 1,
            width: Some(300),
            height: Some(100),
            path: PathBuf::from("wide.png"),
            extension: "png".to_string(),
        },
    ];

    let candidates = image_figure_candidates(&pages_by_number, &images);

    assert_eq!(candidates[0].region.expect("tall region").top, 300);
    assert_eq!(
        spans_text(&candidates[0].caption.expect("caption").spans),
        "Figure 2. Tall image."
    );
    assert_eq!(candidates[1].region.expect("wide region").top, 100);
}

#[test]
fn regionless_small_and_repeated_images_are_reported_not_emitted() {
    let dir = tempfile::tempdir().expect("temp dir");
    let repeated = b"repeated ornament bytes";
    let mut images = Vec::new();
    let mut pages = Vec::new();
    for page_number in 1..=3 {
        pages.push(empty_page(page_number));
        let path = dir.path().join(format!("repeat-{page_number}.png"));
        fs::write(&path, repeated).expect("image bytes");
        images.push(ExtractedImage {
            page: page_number,
            index: page_number as usize - 1,
            width: Some(200),
            height: Some(200),
            path,
            extension: "png".to_string(),
        });
    }
    pages.push(empty_page(4));
    let small_path = dir.path().join("small.png");
    fs::write(&small_path, b"small").expect("small image bytes");
    images.push(ExtractedImage {
        page: 4,
        index: 3,
        width: Some(20),
        height: Some(20),
        path: small_path,
        extension: "png".to_string(),
    });
    let tools = PopplerTools {
        pdftohtml: PathBuf::from("pdftohtml"),
        pdftotext: PathBuf::from("pdftotext"),
        pdfimages: None,
        pdftoppm: None,
    };
    let page_dir = dir.path().join("pages");
    let mut renderer = PageCropRenderer::new(Path::new("input.pdf"), &tools, &page_dir);

    let result = figure_blocks_from_images(&pages, &[], &images, &mut renderer, dir.path())
        .expect("figure pass");

    assert!(result.figures.is_empty());
    assert!(
        result
            .warnings
            .iter()
            .any(|warning| warning.contains("repeated regionless image"))
    );
    assert!(
        result
            .warnings
            .iter()
            .any(|warning| warning.contains("below minimum area"))
    );
}

#[test]
fn parenthetical_stats_fragment_is_not_display_equation() {
    let page = empty_page(1);
    let stats = fragment(200, 245, 110, 12, "(p = 0.05)");

    assert!(!is_display_equation_fragment(&page, &stats));
}

#[test]
fn mnli_table_header_fragment_is_not_display_equation() {
    let page = empty_page(1);
    let header = fragment(200, 220, 160, 12, "MNLI-(m/mm) 392k");

    assert!(!is_display_equation_fragment(&page, &header));
}

#[test]
fn three_row_aligned_stats_without_caption_are_not_tables() {
    let mut page = empty_page(1);
    page.fragments = vec![
        fragment(100, 100, 40, 12, "Mean"),
        fragment(100, 180, 30, 12, "12"),
        fragment(100, 240, 30, 12, "14"),
        fragment(120, 100, 40, 12, "SD"),
        fragment(120, 180, 30, 12, "3"),
        fragment(120, 240, 30, 12, "4"),
        fragment(140, 100, 40, 12, "N"),
        fragment(140, 180, 30, 12, "20"),
        fragment(140, 240, 30, 12, "21"),
    ];

    assert!(table_regions_for_page(&page, &[]).is_empty());
}

#[test]
fn table_region_clamps_to_table_column_on_two_column_page() {
    let mut page = empty_page(1);
    page.fragments = vec![
        fragment(
            70,
            72,
            220,
            10,
            "Left column prose establishes a normal sentence.",
        ),
        fragment(
            86,
            72,
            220,
            10,
            "Another left column sentence keeps detection stable.",
        ),
        fragment(
            70,
            330,
            220,
            10,
            "Right column prose establishes a normal sentence.",
        ),
        fragment(
            86,
            330,
            220,
            10,
            "Another right column sentence keeps detection stable.",
        ),
        fragment(130, 72, 140, 10, "Table 1. Scores."),
        fragment(170, 72, 40, 10, "Metric"),
        fragment(170, 150, 32, 10, "2019"),
        fragment(170, 220, 32, 10, "2020"),
        fragment(
            170,
            330,
            220,
            10,
            "Right column prose should not enter this crop.",
        ),
        fragment(190, 72, 20, 10, "A"),
        fragment(190, 150, 32, 10, "0.91"),
        fragment(190, 220, 32, 10, "0.93"),
        fragment(
            190,
            330,
            220,
            10,
            "Neighboring body text remains ordinary prose.",
        ),
        fragment(210, 72, 20, 10, "B"),
        fragment(210, 150, 32, 10, "0.81"),
        fragment(210, 220, 32, 10, "0.84"),
        fragment(
            210,
            330,
            220,
            10,
            "The crop must stay within the table column.",
        ),
    ];

    let regions = table_regions_for_page(&page, &[]);

    assert_eq!(regions.len(), 1);
    let rect = regions[0].rect;
    assert!(
        rect.right() <= 330,
        "table crop must stay in the table column: {rect:?}"
    );
    assert!(
        page.fragments
            .iter()
            .filter(|fragment| fragment.left > page.width / 2)
            .all(|fragment| !rect.overlaps_fragment(fragment)),
        "right-column prose must stay outside the table crop: {rect:?}"
    );
}

#[test]
fn figure_token_rows_inside_image_region_are_not_tables() {
    let pages = parse_pdf2xml(BERT_FIGURE1_TOKEN_STRIP_XML).expect("fixture parses");
    let regions = detect_media_regions(&pages, &[]);

    assert!(
        regions.iter().all(|region| region.kind != MediaKind::Table),
        "token rows inside Figure 1 image bounds must not become table crops"
    );
}

#[test]
fn vector_chart_region_clamps_to_chart_column_and_excludes_prose() {
    let pages = parse_pdf2xml(BERT_PAGE16_VECTOR_CHART_TWOCOL_XML).expect("fixture parses");
    let page = &pages[0];
    let mut warnings = Vec::new();
    let regions = vector_figure_regions(&pages, &HashSet::from([page.number]), &mut warnings);

    assert_eq!(regions.len(), 1);
    let rect = regions[0].rect;
    assert!(
        rect.right() <= page.width / 2,
        "two-column chart region must stay in the caption column: {rect:?}"
    );
    assert!(
        page.fragments
            .iter()
            .filter(|fragment| is_prose_like_fragment(fragment))
            .all(|fragment| !rect.overlaps_fragment(fragment)),
        "prose fragments must stay outside vector chart text region: {rect:?}"
    );
    let crop = padded_vector_crop_rect(page, rect);
    assert_eq!(
        crop.top, rect.top,
        "vector crop must not pad upward into prose above the chart"
    );
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
}

#[test]
fn vector_chart_region_drops_short_prose_above_chart_labels() {
    let mut page = empty_page(1);
    page.width = 892;
    page.height = 1262;
    page.fragments = vec![
        fragment(
            100,
            108,
            327,
            15,
            "Left column prose establishes normal text for detection.",
        ),
        fragment(
            120,
            108,
            327,
            15,
            "Another left column sentence keeps this page two-column.",
        ),
        fragment(
            100,
            461,
            327,
            15,
            "Right column prose establishes normal text for detection.",
        ),
        fragment(
            120,
            461,
            327,
            15,
            "Another right column sentence keeps this page two-column.",
        ),
        fragment(711, 108, 71, 15, "In Section"),
        fragment(
            731,
            108,
            327,
            15,
            "mixed strategy for masking the target tokens when",
        ),
        fragment(812, 108, 66, 15, "strategies."),
        fragment(871, 130, 12, 9, "84"),
        fragment(904, 130, 12, 9, "82"),
        fragment(936, 130, 12, 9, "80"),
        fragment(1026, 188, 18, 9, "200"),
        fragment(1043, 205, 130, 9, "Pre-training Steps"),
        fragment(
            1075,
            108,
            327,
            13,
            "Figure 5: Ablation over number of training steps.",
        ),
    ];
    let mut warnings = Vec::new();
    let pages = vec![page];

    let regions = vector_figure_regions(&pages, &HashSet::from([1]), &mut warnings);

    assert_eq!(regions.len(), 1);
    assert!(
        regions[0].rect.top >= 871,
        "chart region must start at chart labels, not short prose: {:?}",
        regions[0].rect
    );
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
}

#[test]
fn full_width_vector_diagram_is_not_clamped_to_one_column() {
    let mut page = empty_page(1);
    page.width = 892;
    page.height = 1262;
    page.fragments = vec![
        fragment(
            399,
            108,
            327,
            15,
            "Left column prose establishes normal text for detection.",
        ),
        fragment(
            419,
            108,
            327,
            15,
            "Another left column sentence keeps this page two-column.",
        ),
        fragment(
            399,
            461,
            327,
            15,
            "Right column prose establishes normal text for detection.",
        ),
        fragment(
            419,
            461,
            327,
            15,
            "Another right column sentence keeps this page two-column.",
        ),
        fragment(101, 167, 79, 13, "BERT (Ours)"),
        fragment(153, 144, 12, 6, "Trm"),
        fragment(153, 178, 12, 6, "Trm"),
        fragment(127, 147, 6, 6, "T"),
        fragment(131, 153, 3, 4, "1"),
        fragment(127, 182, 4, 6, "T"),
        fragment(131, 186, 3, 4, "2"),
        fragment(102, 335, 79, 13, "OpenAI GPT"),
        fragment(153, 322, 12, 6, "Trm"),
        fragment(153, 357, 12, 6, "Trm"),
        fragment(127, 326, 6, 6, "T"),
        fragment(131, 332, 3, 4, "1"),
        fragment(105, 608, 36, 13, "ELMo"),
        fragment(165, 486, 15, 6, "Lstm"),
        fragment(165, 683, 15, 6, "Lstm"),
        fragment(165, 749, 15, 6, "Lstm"),
        fragment(127, 573, 6, 6, "T"),
        fragment(131, 579, 3, 4, "1"),
        fragment(127, 608, 4, 6, "T"),
        fragment(131, 612, 3, 4, "2"),
        fragment(228, 678, 6, 6, "E"),
        fragment(233, 685, 3, 4, "N"),
        fragment(
            276,
            108,
            680,
            13,
            "Figure 3: Differences in pre-training model architectures.",
        ),
    ];
    let mut warnings = Vec::new();
    let page_width = page.width;
    let pages = vec![page];

    let regions = vector_figure_regions(&pages, &HashSet::from([1]), &mut warnings);

    assert_eq!(regions.len(), 1);
    assert!(
        regions[0].rect.right() > page_width * 3 / 4,
        "full-width vector diagram must include the rightmost panel: {:?}",
        regions[0].rect
    );
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
}

#[test]
fn vertically_overlapping_outer_subimage_clusters_merge() {
    let mut page = empty_page(1);
    page.images = vec![
        ImageRegion {
            top: 120,
            left: 60,
            width: 40,
            height: 30,
            src: None,
        },
        ImageRegion {
            top: 124,
            left: 180,
            width: 40,
            height: 30,
            src: None,
        },
        ImageRegion {
            top: 126,
            left: 235,
            width: 40,
            height: 30,
            src: None,
        },
        ImageRegion {
            top: 122,
            left: 430,
            width: 40,
            height: 30,
            src: None,
        },
    ];
    let pages_by_number = HashMap::from([(1, &page)]);
    let images = (0..4)
        .map(|index| ExtractedImage {
            page: 1,
            index,
            width: Some(40),
            height: Some(30),
            path: PathBuf::from(format!("subimage-{index}.png")),
            extension: "png".to_string(),
        })
        .collect::<Vec<_>>();
    let candidates = image_figure_candidates(&pages_by_number, &images);
    let clusters = image_candidate_clusters(
        &candidates,
        &vec![false; candidates.len()],
        &pages_by_number,
    );

    assert_eq!(
        clusters.len(),
        1,
        "expected one merged cluster: {clusters:?}"
    );
    let mut cluster = clusters[0].clone();
    cluster.sort_unstable();
    assert_eq!(cluster, vec![0, 1, 2, 3]);
}

#[cfg(unix)]
#[test]
fn nearby_raster_subimages_cluster_into_one_figure() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut page = empty_page(1);
    page.images = vec![
        ImageRegion {
            top: 120,
            left: 100,
            width: 40,
            height: 30,
            src: None,
        },
        ImageRegion {
            top: 124,
            left: 160,
            width: 42,
            height: 30,
            src: None,
        },
        ImageRegion {
            top: 126,
            left: 222,
            width: 40,
            height: 30,
            src: None,
        },
    ];
    page.fragments = vec![fragment(
        190,
        100,
        320,
        12,
        "Figure 1. A diagram composed of raster sub-images.",
    )];
    let mut images = Vec::new();
    for index in 0..3 {
        let path = dir.path().join(format!("subimage-{index}.png"));
        fs::write(&path, format!("subimage-{index}")).expect("image bytes");
        images.push(ExtractedImage {
            page: 1,
            index,
            width: Some(40),
            height: Some(30),
            path,
            extension: "png".to_string(),
        });
    }
    let pdftoppm = dir.path().join("pdftoppm");
    fake_pdftoppm(&pdftoppm);
    let tools = PopplerTools {
        pdftohtml: PathBuf::from("pdftohtml"),
        pdftotext: PathBuf::from("pdftotext"),
        pdfimages: None,
        pdftoppm: Some(pdftoppm),
    };
    let page_dir = dir.path().join("pages");
    let mut renderer = PageCropRenderer::new(Path::new("input.pdf"), &tools, &page_dir);

    let result = figure_blocks_from_images(&[page], &[], &images, &mut renderer, dir.path())
        .expect("figure pass");

    assert_eq!(result.figures.len(), 1);
    assert_eq!(result.exclusions.len(), 1);
    let DocBlock::Figure { image, caption } = &result.figures[0].block else {
        panic!("cluster should emit a figure block");
    };
    assert_eq!(image.id, "pdf-figure-0001");
    assert_eq!(
        caption.as_ref().map(|spans| spans_text(spans)).as_deref(),
        Some("Figure 1. A diagram composed of raster sub-images.")
    );
}

#[test]
fn caption_ranking_prefers_closer_caption_below_image_bottom() {
    let mut page = empty_page(1);
    page.fragments = vec![
        fragment(193, 100, 220, 12, "Figure 1. Above decoy."),
        fragment(202, 100, 220, 12, "Figure 2. True caption."),
    ];
    let region = ImageRegion {
        top: 100,
        left: 100,
        width: 100,
        height: 100,
        src: None,
    };

    let caption = detect_caption_fragment(&page, Some(&region)).expect("caption");

    assert_eq!(spans_text(&caption.spans), "Figure 2. True caption.");
}

#[cfg(unix)]
#[cfg(unix)]
#[test]
fn convert_pdf_does_not_write_epub_when_baseline_fails() {
    use std::fs;

    let dir = tempfile::tempdir().expect("temp dir");
    let input = dir.path().join("input.pdf");
    let output = dir.path().join("output.epub");
    fs::write(&input, b"dummy pdf").expect("input pdf fixture");

    let pdftohtml = dir.path().join("pdftohtml");
    write_executable(
        &pdftohtml,
        r##"#!/bin/sh
cat <<'XML'
<pdf2xml>
  <page number="1" width="600" height="800">
    <fontspec id="0" size="12" family="Times" color="#000000"/>
    <text top="100" left="100" width="80" height="12" font="0">Hello PDF</text>
  </page>
</pdf2xml>
XML
"##,
    );

    let pdftotext = dir.path().join("pdftotext");
    write_executable(
        &pdftotext,
        r#"#!/bin/sh
echo baseline failed >&2
exit 9
"#,
    );
    let pdfimages = dir.path().join("pdfimages");
    fake_pdfimages(&pdfimages);
    let pdftoppm = dir.path().join("pdftoppm");
    fake_pdftoppm(&pdftoppm);

    let tools = PopplerTools {
        pdftohtml,
        pdftotext,
        pdfimages: Some(pdfimages),
        pdftoppm: Some(pdftoppm),
    };

    let result = convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), &tools, None);

    assert!(result.is_err());
    assert!(
        !output.exists(),
        "output EPUB should not be written after baseline failure"
    );
}

#[cfg(unix)]
#[test]
fn convert_pdf_continues_text_only_without_optional_image_tools() {
    use std::{fs, io::Read};
    use zip::ZipArchive;

    let dir = tempfile::tempdir().expect("temp dir");
    let input = dir.path().join("input.pdf");
    let output = dir.path().join("output.epub");
    fs::write(&input, b"dummy pdf").expect("input pdf fixture");

    let pdftohtml = dir.path().join("pdftohtml");
    write_executable(
        &pdftohtml,
        r##"#!/bin/sh
cat <<'XML'
<pdf2xml>
  <page number="1" width="600" height="800">
    <fontspec id="0" size="12" family="Times" color="#000000"/>
    <text top="100" left="100" width="180" height="12" font="0">Text only PDF.</text>
  </page>
</pdf2xml>
XML
"##,
    );
    let pdftotext = dir.path().join("pdftotext");
    fake_pdftotext_with_text(&pdftotext, "Text only PDF.\n");
    let tools = PopplerTools {
        pdftohtml,
        pdftotext,
        pdfimages: None,
        pdftoppm: None,
    };

    let outcome = convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), &tools, None)
        .expect("conversion should succeed without optional image tools");

    assert!(output.exists());
    assert!(
        outcome
            .report
            .warnings
            .iter()
            .any(|warning| warning.contains("image extraction unavailable"))
    );
    let mut archive =
        ZipArchive::new(fs::File::open(&output).expect("epub opens")).expect("zip opens");
    let mut content = String::new();
    archive
        .by_name("content.xhtml")
        .expect("content exists")
        .read_to_string(&mut content)
        .expect("content reads");
    assert!(content.contains("Text only PDF."));
}

#[cfg(unix)]
#[cfg(unix)]
#[test]
fn convert_pdf_embeds_extracted_image_with_translatable_caption() {
    use std::{fs, io::Read};
    use zip::ZipArchive;

    let dir = tempfile::tempdir().expect("temp dir");
    let input = dir.path().join("input.pdf");
    let output = dir.path().join("output.epub");
    fs::write(&input, b"dummy pdf").expect("input pdf fixture");

    let pdftohtml = dir.path().join("pdftohtml");
    write_executable(
        &pdftohtml,
        r##"#!/bin/sh
cat <<'XML'
<pdf2xml>
  <page number="1" width="600" height="800">
    <fontspec id="0" size="14" family="Times" color="#000000"/>
    <fontspec id="1" size="12" family="Times" color="#000000"/>
    <text top="80" left="100" width="300" height="16" font="0">Paper Title</text>
    <image top="130" left="120" width="120" height="80" src="paper-1_1.png"/>
    <text top="218" left="120" width="260" height="12" font="1">Figure 1. A test image.</text>
    <text top="280" left="100" width="300" height="12" font="1">Body text after the figure.</text>
  </page>
</pdf2xml>
XML
"##,
    );

    let pdftotext = dir.path().join("pdftotext");
    write_executable(
        &pdftotext,
        r#"#!/bin/sh
printf 'Paper Title\nFigure 1. A test image.\nBody text after the figure.\n'
"#,
    );
    let pdfimages = dir.path().join("pdfimages");
    fake_pdfimages(&pdfimages);
    let pdftoppm = dir.path().join("pdftoppm");
    fake_pdftoppm(&pdftoppm);

    let tools = PopplerTools {
        pdftohtml,
        pdftotext,
        pdfimages: Some(pdfimages),
        pdftoppm: Some(pdftoppm),
    };
    let outcome = convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), &tools, None)
        .expect("conversion should succeed");

    assert_eq!(outcome.report.images, 1);
    assert_eq!(outcome.report.figures, 1, "{}", outcome.report.summary());

    let mut archive =
        ZipArchive::new(fs::File::open(&output).expect("epub opens")).expect("zip opens");
    let mut content = String::new();
    archive
        .by_name("content.xhtml")
        .expect("content exists")
        .read_to_string(&mut content)
        .expect("content reads");
    assert!(content.contains("<figure id=\"pdf-image-0001\">"));
    assert!(content.contains("<figcaption>Figure 1. A test image.</figcaption>"));
    assert_eq!(content.matches("Figure 1. A test image.").count(), 1);

    let mut image = Vec::new();
    archive
        .by_name("images/pdf-image-0001.png")
        .expect("image exists")
        .read_to_end(&mut image)
        .expect("image reads");
    assert_eq!(image, b"fake-image");

    let book = bookforge_epub::read_epub(&output).expect("converted EPUB should be readable");
    assert!(
        book.blocks
            .iter()
            .any(|block| matches!(block.kind, bookforge_core::ir::BlockKind::Caption)),
        "figcaption should remain a normal translatable caption block"
    );
}

#[cfg(unix)]
#[cfg(unix)]
#[test]
fn convert_pdf_snaps_figure_crop_above_caption_boundary_fixture() {
    use std::{fs, io::Read};
    use zip::ZipArchive;

    let dir = tempfile::tempdir().expect("temp dir");
    let input = dir.path().join("input.pdf");
    let output = dir.path().join("output.epub");
    fs::write(&input, b"dummy pdf").expect("input pdf fixture");

    let pdftohtml = dir.path().join("pdftohtml");
    fake_pdftohtml_with_xml(&pdftohtml, BERT_FIGURE1_CAPTION_BOUNDARY_XML);
    let pdftotext = dir.path().join("pdftotext");
    fake_pdftotext_with_text(
        &pdftotext,
        "Results introduce a boundary-sensitive figure.\nFigure 1. Boundary-sensitive raster caption.\nThe caption should be translated once.\n",
    );
    let pdfimages = dir.path().join("pdfimages");
    fake_pdfimages_two(&pdfimages);
    let pdftoppm = dir.path().join("pdftoppm");
    fake_pdftoppm(&pdftoppm);

    let tools = PopplerTools {
        pdftohtml,
        pdftotext,
        pdfimages: Some(pdfimages),
        pdftoppm: Some(pdftoppm),
    };
    let outcome = convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), &tools, None)
        .expect("conversion should succeed");

    assert_eq!(outcome.report.images, 2);
    assert_eq!(outcome.report.figures, 1, "{}", outcome.report.summary());
    assert!(
        outcome
            .report
            .warnings
            .iter()
            .any(|warning| warning.contains("figure crop snapped above caption")),
        "caption boundary crop should report the snap"
    );

    let mut archive =
        ZipArchive::new(fs::File::open(&output).expect("epub opens")).expect("zip opens");
    let mut content = String::new();
    archive
        .by_name("content.xhtml")
        .expect("content exists")
        .read_to_string(&mut content)
        .expect("content reads");
    assert!(content.contains("<figure id=\"pdf-figure-0001\">"));
    assert_eq!(
        content
            .matches("Figure 1. Boundary-sensitive raster caption.")
            .count(),
        1
    );

    let mut image = Vec::new();
    archive
        .by_name("images/pdf-figure-0001.png")
        .expect("figure crop exists")
        .read_to_end(&mut image)
        .expect("crop reads");
    assert!(image.starts_with(b"\x89PNG\r\n\x1a\n"));
}

#[cfg(unix)]
#[cfg(unix)]
#[test]
fn convert_pdf_groups_multipanel_figure_fixture_as_one_captioned_crop() {
    use std::{fs, io::Read};
    use zip::ZipArchive;

    let dir = tempfile::tempdir().expect("temp dir");
    let input = dir.path().join("input.pdf");
    let output = dir.path().join("output.epub");
    fs::write(&input, b"dummy pdf").expect("input pdf fixture");

    let pdftohtml = dir.path().join("pdftohtml");
    fake_pdftohtml_with_xml(&pdftohtml, BERT_FIGURE4_MULTIPANEL_XML);
    let pdftotext = dir.path().join("pdftotext");
    fake_pdftotext_with_text(
        &pdftotext,
        "A multi-panel result follows.\nFigure 4. Multi-panel activation maps.\nDiscussion resumes after the figure.\n",
    );
    let pdfimages = dir.path().join("pdfimages");
    fake_pdfimages_two(&pdfimages);
    let pdftoppm = dir.path().join("pdftoppm");
    fake_pdftoppm(&pdftoppm);

    let tools = PopplerTools {
        pdftohtml,
        pdftotext,
        pdfimages: Some(pdfimages),
        pdftoppm: Some(pdftoppm),
    };
    let outcome = convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), &tools, None)
        .expect("conversion should succeed");

    assert_eq!(outcome.report.images, 2);
    assert_eq!(outcome.report.figures, 1, "{}", outcome.report.summary());

    let mut archive =
        ZipArchive::new(fs::File::open(&output).expect("epub opens")).expect("zip opens");
    let mut content = String::new();
    archive
        .by_name("content.xhtml")
        .expect("content exists")
        .read_to_string(&mut content)
        .expect("content reads");
    assert!(content.contains("<figure id=\"pdf-figure-0001\">"));
    assert!(!content.contains("pdf-image-0001"));
    assert!(!content.contains("pdf-image-0002"));
    assert_eq!(
        content
            .matches("Figure 4. Multi-panel activation maps.")
            .count(),
        1
    );
    assert!(content.contains("Discussion resumes after the figure."));
}

#[cfg(unix)]
#[cfg(unix)]
#[test]
fn convert_pdf_preserves_vector_chart_fixture_as_captioned_crop() {
    use std::{fs, io::Read};
    use zip::ZipArchive;

    let dir = tempfile::tempdir().expect("temp dir");
    let input = dir.path().join("input.pdf");
    let output = dir.path().join("output.epub");
    fs::write(&input, b"dummy pdf").expect("input pdf fixture");

    let pdftohtml = dir.path().join("pdftohtml");
    fake_pdftohtml_with_xml(&pdftohtml, BERT_FIGURE5_VECTOR_CHART_XML);
    let pdftotext = dir.path().join("pdftotext");
    fake_pdftotext_with_text(
        &pdftotext,
        "Vector-chart results are below.\n1.0 0.5 0.0 0 10 20 Epoch\nFigure 5. Vector chart of held-out accuracy.\nThe next paragraph should stay prose.\n",
    );
    let pdfimages = dir.path().join("pdfimages");
    fake_pdfimages_empty(&pdfimages);
    let pdftoppm = dir.path().join("pdftoppm");
    fake_pdftoppm(&pdftoppm);

    let tools = PopplerTools {
        pdftohtml,
        pdftotext,
        pdfimages: Some(pdfimages),
        pdftoppm: Some(pdftoppm),
    };
    let outcome = convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), &tools, None)
        .expect("conversion should succeed");

    assert_eq!(outcome.report.images, 0);
    assert_eq!(outcome.report.figures, 1, "{}", outcome.report.summary());
    assert_eq!(outcome.report.tables, 0);
    assert_eq!(outcome.report.equations, 0);

    let mut archive =
        ZipArchive::new(fs::File::open(&output).expect("epub opens")).expect("zip opens");
    let mut content = String::new();
    archive
        .by_name("content.xhtml")
        .expect("content exists")
        .read_to_string(&mut content)
        .expect("content reads");
    assert!(content.contains("<figure id=\"pdf-figure-0001\">"));
    assert_eq!(
        content
            .matches("Figure 5. Vector chart of held-out accuracy.")
            .count(),
        1
    );
    assert!(content.contains("The next paragraph should stay prose."));
    assert!(!content.contains("1.0 0.5"));
    assert!(!content.contains("Epoch"));
}

#[cfg(unix)]
#[cfg(unix)]
#[test]
fn convert_pdf_warns_on_lowercase_continuation_after_media() {
    use std::fs;

    let dir = tempfile::tempdir().expect("temp dir");
    let input = dir.path().join("input.pdf");
    let output = dir.path().join("output.epub");
    fs::write(&input, b"dummy pdf").expect("input pdf fixture");

    let pdftohtml = dir.path().join("pdftohtml");
    write_executable(
        &pdftohtml,
        r##"#!/bin/sh
cat <<'XML'
<pdf2xml>
  <page number="1" width="600" height="800">
    <fontspec id="0" size="12" family="Times" color="#000000"/>
    <text top="80" left="100" width="260" height="12" font="0">This paragraph</text>
    <image top="130" left="120" width="120" height="80" src="paper-1_1.png"/>
    <text top="260" left="100" width="260" height="12" font="0">continues after the figure.</text>
  </page>
</pdf2xml>
XML
"##,
    );

    let pdftotext = dir.path().join("pdftotext");
    write_executable(
        &pdftotext,
        r#"#!/bin/sh
printf 'This paragraph\ncontinues after the figure.\n'
"#,
    );
    let pdfimages = dir.path().join("pdfimages");
    fake_pdfimages(&pdfimages);
    let pdftoppm = dir.path().join("pdftoppm");
    fake_pdftoppm(&pdftoppm);

    let tools = PopplerTools {
        pdftohtml,
        pdftotext,
        pdfimages: Some(pdfimages),
        pdftoppm: Some(pdftoppm),
    };
    let outcome = convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), &tools, None)
        .expect("conversion should succeed");

    assert!(
            outcome
                .report
                .warnings
                .iter()
                .any(|warning| warning
                    .contains("lowercase paragraph continuation follows media block")),
            "media-separated lowercase continuations should be flagged"
        );
}

#[cfg(unix)]
#[cfg(unix)]
#[test]
fn convert_pdf_preserves_detected_table_as_crop_with_caption() {
    use std::{fs, io::Read};
    use zip::ZipArchive;

    let dir = tempfile::tempdir().expect("temp dir");
    let input = dir.path().join("input.pdf");
    let output = dir.path().join("output.epub");
    fs::write(&input, b"dummy pdf").expect("input pdf fixture");

    let pdftohtml = dir.path().join("pdftohtml");
    write_executable(
        &pdftohtml,
        r##"#!/bin/sh
cat <<'XML'
<pdf2xml>
  <page number="1" width="600" height="800">
    <fontspec id="0" size="12" family="Times" color="#000000"/>
    <text top="80" left="100" width="260" height="12" font="0">Body before table.</text>
    <text top="120" left="100" width="220" height="12" font="0">Table 1. Scores.</text>
    <text top="170" left="100" width="50" height="12" font="0">Metric</text>
    <text top="170" left="240" width="40" height="12" font="0">2019</text>
    <text top="170" left="360" width="40" height="12" font="0">2020</text>
    <text top="190" left="100" width="20" height="12" font="0">A</text>
    <text top="190" left="240" width="40" height="12" font="0">0.91</text>
    <text top="190" left="360" width="40" height="12" font="0">0.93</text>
    <text top="210" left="100" width="20" height="12" font="0">B</text>
    <text top="210" left="240" width="40" height="12" font="0">0.81</text>
    <text top="210" left="360" width="40" height="12" font="0">0.84</text>
    <text top="280" left="100" width="260" height="12" font="0">Body after table.</text>
  </page>
</pdf2xml>
XML
"##,
    );

    let pdftotext = dir.path().join("pdftotext");
    write_executable(
        &pdftotext,
        r#"#!/bin/sh
printf 'Body before table.\nTable 1. Scores.\nMetric 2019 2020\nA 0.91 0.93\nB 0.81 0.84\nBody after table.\n'
"#,
    );
    let pdfimages = dir.path().join("pdfimages");
    fake_pdfimages_empty(&pdfimages);
    let pdftoppm = dir.path().join("pdftoppm");
    fake_pdftoppm(&pdftoppm);

    let tools = PopplerTools {
        pdftohtml,
        pdftotext,
        pdfimages: Some(pdfimages),
        pdftoppm: Some(pdftoppm),
    };
    let outcome = convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), &tools, None)
        .expect("conversion should succeed");

    assert_eq!(outcome.report.tables, 1);
    assert_eq!(outcome.report.equations, 0);
    assert_eq!(outcome.report.figures, 1);
    assert!(outcome.report.media_preserved_chars > 0);
    assert_eq!(outcome.report.coverage_percent, 100.0);
    assert!(
        outcome
            .report
            .warnings
            .iter()
            .any(|warning| warning.contains("preserved as image near y="))
    );
    assert!(
        !outcome
            .report
            .warnings
            .iter()
            .any(|warning| warning.contains("reconstructed text covers only"))
    );

    let mut archive =
        ZipArchive::new(fs::File::open(&output).expect("epub opens")).expect("zip opens");
    let mut content = String::new();
    archive
        .by_name("content.xhtml")
        .expect("content exists")
        .read_to_string(&mut content)
        .expect("content reads");
    assert!(content.contains("Body before table."));
    assert!(content.contains("Body after table."));
    assert!(content.contains("<figure id=\"pdf-table-0001\">"));
    assert!(content.contains("<figcaption>Table 1. Scores.</figcaption>"));
    assert_eq!(content.matches("Table 1. Scores.").count(), 1);
    assert!(!content.contains("0.91"));
    assert!(!content.contains("2019"));

    let mut image = Vec::new();
    archive
        .by_name("images/pdf-table-0001.png")
        .expect("table crop exists")
        .read_to_end(&mut image)
        .expect("image reads");
    assert!(image.starts_with(b"\x89PNG\r\n\x1a\n"));
}

#[cfg(unix)]
#[cfg(unix)]
#[test]
fn convert_pdf_preserves_display_equation_as_crop() {
    use std::{fs, io::Read};
    use zip::ZipArchive;

    let dir = tempfile::tempdir().expect("temp dir");
    let input = dir.path().join("input.pdf");
    let output = dir.path().join("output.epub");
    fs::write(&input, b"dummy pdf").expect("input pdf fixture");

    let pdftohtml = dir.path().join("pdftohtml");
    write_executable(
        &pdftohtml,
        r##"#!/bin/sh
cat <<'XML'
<pdf2xml>
  <page number="1" width="600" height="800">
    <fontspec id="0" size="12" family="Times" color="#000000"/>
    <text top="80" left="100" width="260" height="12" font="0">Body before equation.</text>
    <text top="160" left="240" width="120" height="12" font="0">E = mc^2</text>
    <text top="230" left="100" width="260" height="12" font="0">Body after equation.</text>
  </page>
</pdf2xml>
XML
"##,
    );

    let pdftotext = dir.path().join("pdftotext");
    write_executable(
        &pdftotext,
        r#"#!/bin/sh
printf 'Body before equation.\nE = mc^2\nBody after equation.\n'
"#,
    );
    let pdfimages = dir.path().join("pdfimages");
    fake_pdfimages_empty(&pdfimages);
    let pdftoppm = dir.path().join("pdftoppm");
    fake_pdftoppm(&pdftoppm);

    let tools = PopplerTools {
        pdftohtml,
        pdftotext,
        pdfimages: Some(pdfimages),
        pdftoppm: Some(pdftoppm),
    };
    let outcome = convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), &tools, None)
        .expect("conversion should succeed");

    assert_eq!(outcome.report.tables, 0);
    assert_eq!(outcome.report.equations, 1);
    assert_eq!(outcome.report.figures, 1);

    let mut archive =
        ZipArchive::new(fs::File::open(&output).expect("epub opens")).expect("zip opens");
    let mut content = String::new();
    archive
        .by_name("content.xhtml")
        .expect("content exists")
        .read_to_string(&mut content)
        .expect("content reads");
    assert!(content.contains("Body before equation."));
    assert!(content.contains("Body after equation."));
    assert!(content.contains("<figure id=\"pdf-equation-0001\">"));
    assert!(content.contains("images/pdf-equation-0001.png"));
    assert!(!content.contains("E = mc^2"));

    let mut image = Vec::new();
    archive
        .by_name("images/pdf-equation-0001.png")
        .expect("equation crop exists")
        .read_to_end(&mut image)
        .expect("image reads");
    assert!(image.starts_with(b"\x89PNG\r\n\x1a\n"));
}

#[cfg(unix)]
#[cfg(unix)]
#[test]
fn convert_pdf_marks_inline_math_as_protected_span() {
    use std::fs;

    let dir = tempfile::tempdir().expect("temp dir");
    let input = dir.path().join("input.pdf");
    let output = dir.path().join("output.epub");
    fs::write(&input, b"dummy pdf").expect("input pdf fixture");

    let pdftohtml = dir.path().join("pdftohtml");
    write_executable(
        &pdftohtml,
        r##"#!/bin/sh
cat <<'XML'
<pdf2xml>
  <page number="1" width="600" height="800">
    <fontspec id="0" size="12" family="Times" color="#000000"/>
    <text top="100" left="100" width="360" height="12" font="0">The energy term E = mc^2 appears inline.</text>
  </page>
</pdf2xml>
XML
"##,
    );

    let pdftotext = dir.path().join("pdftotext");
    write_executable(
        &pdftotext,
        r#"#!/bin/sh
printf 'The energy term E = mc^2 appears inline.\n'
"#,
    );
    let pdfimages = dir.path().join("pdfimages");
    fake_pdfimages_empty(&pdfimages);
    let pdftoppm = dir.path().join("pdftoppm");
    fake_pdftoppm(&pdftoppm);

    let tools = PopplerTools {
        pdftohtml,
        pdftotext,
        pdfimages: Some(pdfimages),
        pdftoppm: Some(pdftoppm),
    };
    convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), &tools, None)
        .expect("conversion should succeed");

    let book = bookforge_epub::read_epub(&output).expect("converted EPUB should be readable");
    assert!(
        book.blocks.iter().any(|block| {
            block.protected_spans.iter().any(|span| {
                span.kind == bookforge_core::ir::ProtectedSpanKind::Math && span.text == "E = mc^2"
            })
        }),
        "inline math should become a protected span after PDF conversion"
    );
}

#[cfg(unix)]
#[cfg(unix)]
#[test]
fn convert_pdf_does_not_crop_model_parameter_prose_as_equation() {
    use std::{fs, io::Read};
    use zip::ZipArchive;

    let dir = tempfile::tempdir().expect("temp dir");
    let input = dir.path().join("input.pdf");
    let output = dir.path().join("output.epub");
    fs::write(&input, b"dummy pdf").expect("input pdf fixture");

    let pdftohtml = dir.path().join("pdftohtml");
    fake_pdftohtml_with_xml(&pdftohtml, BERT_MODEL_PARAMETER_FALSE_POSITIVE_XML);

    let pdftotext = dir.path().join("pdftotext");
    fake_pdftotext_with_text(
        &pdftotext,
        "The model = stable result used k = 3 in parentheses (n = 12).\n",
    );
    let pdfimages = dir.path().join("pdfimages");
    fake_pdfimages_empty(&pdfimages);
    let pdftoppm = dir.path().join("pdftoppm");
    fake_pdftoppm(&pdftoppm);

    let tools = PopplerTools {
        pdftohtml,
        pdftotext,
        pdfimages: Some(pdfimages),
        pdftoppm: Some(pdftoppm),
    };
    let outcome = convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), &tools, None)
        .expect("conversion should succeed");

    assert_eq!(outcome.report.tables, 0);
    assert_eq!(outcome.report.equations, 0);
    assert_eq!(outcome.report.figures, 0);

    let mut archive =
        ZipArchive::new(fs::File::open(&output).expect("epub opens")).expect("zip opens");
    let mut content = String::new();
    archive
        .by_name("content.xhtml")
        .expect("content exists")
        .read_to_string(&mut content)
        .expect("content reads");
    assert!(content.contains("The model = stable result used k = 3"));
    assert!(!content.contains("pdf-equation-0001.png"));
}

#[cfg(unix)]
#[cfg(unix)]
#[test]
fn low_confidence_pages_linearize_by_default() {
    use std::{fs, io::Read};
    use zip::ZipArchive;

    let dir = tempfile::tempdir().expect("temp dir");
    let input = dir.path().join("input.pdf");
    let output = dir.path().join("output.epub");
    fs::write(&input, b"dummy pdf").expect("input pdf fixture");

    let pdftohtml = dir.path().join("pdftohtml");
    write_executable(
        &pdftohtml,
        r##"#!/bin/sh
cat <<'XML'
<pdf2xml>
  <page number="1" width="600" height="800">
    <fontspec id="0" size="12" family="Times" color="#000000"/>
    <text top="100" left="100" width="20" height="12" font="0">Tiny</text>
  </page>
</pdf2xml>
XML
"##,
    );

    let pdftotext = dir.path().join("pdftotext");
    write_executable(
        &pdftotext,
        r#"#!/bin/sh
printf 'Tiny plus many baseline characters that the XML reconstruction did not recover.\n'
"#,
    );
    let pdfimages = dir.path().join("pdfimages");
    fake_pdfimages_empty(&pdfimages);
    let pdftoppm = dir.path().join("pdftoppm");
    fake_pdftoppm(&pdftoppm);

    let tools = PopplerTools {
        pdftohtml,
        pdftotext,
        pdfimages: Some(pdfimages),
        pdftoppm: Some(pdftoppm),
    };
    let outcome = convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), &tools, None)
        .expect("conversion should succeed");

    assert_eq!(outcome.report.low_confidence_pages, 1);
    assert_eq!(
        outcome.report.page_stats[0]
            .low_confidence_action
            .as_deref(),
        Some("linearize")
    );
    assert!(
        outcome
            .report
            .warnings
            .iter()
            .any(|warning| warning.contains("page 1: low-confidence")
                && warning.contains("action=linearize"))
    );

    let mut archive =
        ZipArchive::new(fs::File::open(&output).expect("epub opens")).expect("zip opens");
    let mut content = String::new();
    archive
        .by_name("content.xhtml")
        .expect("content exists")
        .read_to_string(&mut content)
        .expect("content reads");
    assert!(content.contains("Tiny"));
    assert!(!content.contains("pdf-page-0001.png"));
}

#[cfg(unix)]
#[cfg(unix)]
#[test]
fn low_confidence_pages_can_be_preserved_as_page_images() {
    use std::{fs, io::Read};
    use zip::ZipArchive;

    let dir = tempfile::tempdir().expect("temp dir");
    let input = dir.path().join("input.pdf");
    let output = dir.path().join("output.epub");
    fs::write(&input, b"dummy pdf").expect("input pdf fixture");

    let pdftohtml = dir.path().join("pdftohtml");
    write_executable(
        &pdftohtml,
        r##"#!/bin/sh
cat <<'XML'
<pdf2xml>
  <page number="1" width="600" height="800">
    <fontspec id="0" size="12" family="Times" color="#000000"/>
    <text top="100" left="100" width="20" height="12" font="0">Tiny</text>
  </page>
</pdf2xml>
XML
"##,
    );

    let pdftotext = dir.path().join("pdftotext");
    write_executable(
        &pdftotext,
        r#"#!/bin/sh
printf 'Tiny plus many baseline characters that the XML reconstruction did not recover.\n'
"#,
    );
    let pdfimages = dir.path().join("pdfimages");
    fake_pdfimages_empty(&pdfimages);
    let pdftoppm = dir.path().join("pdftoppm");
    fake_pdftoppm(&pdftoppm);

    let tools = PopplerTools {
        pdftohtml,
        pdftotext,
        pdfimages: Some(pdfimages),
        pdftoppm: Some(pdftoppm),
    };
    let options = ConvertOptions {
        low_confidence: LowConfidenceMode::Preserve,
        ..ConvertOptions::default()
    };
    let outcome = convert_pdf_with_tools(&input, &output, &options, &tools, None)
        .expect("conversion should succeed");

    assert_eq!(outcome.report.low_confidence_pages, 1);
    assert_eq!(
        outcome.report.page_stats[0]
            .low_confidence_action
            .as_deref(),
        Some("preserve")
    );
    assert!(
        outcome
            .report
            .warnings
            .iter()
            .any(|warning| warning.contains("page 1: low-confidence")
                && warning.contains("action=preserve"))
    );

    let mut archive =
        ZipArchive::new(fs::File::open(&output).expect("epub opens")).expect("zip opens");
    let mut content = String::new();
    archive
        .by_name("content.xhtml")
        .expect("content exists")
        .read_to_string(&mut content)
        .expect("content reads");
    assert!(content.contains("<figure id=\"pdf-page-0001\">"));
    assert!(content.contains("images/pdf-page-0001.png"));
    assert!(!content.contains("Tiny"));

    let mut image = Vec::new();
    archive
        .by_name("images/pdf-page-0001.png")
        .expect("preserved page image exists")
        .read_to_end(&mut image)
        .expect("image reads");
    assert!(image.starts_with(b"\x89PNG\r\n\x1a\n"));
}
