use super::*;

use crate::convert::fake_backend::{FakePoppler, FixtureImage};

// pdftohtml fixture documents. The shell-backed cases are Unix-only; the
// portable in-process cases below use `FakePoppler`.
#[cfg(unix)]
const BERT_FIGURE1_CAPTION_BOUNDARY_XML: &str =
    include_str!("../../fixtures/bert_figure1_caption_boundary.xml");
#[cfg(unix)]
const BERT_FIGURE4_MULTIPANEL_XML: &str =
    include_str!("../../fixtures/bert_figure4_multipanel.xml");
const BERT_FIGURE5_VECTOR_CHART_XML: &str =
    include_str!("../../fixtures/bert_figure5_vector_chart.xml");
const BERT_PAGE16_VECTOR_CHART_TWOCOL_XML: &str =
    include_str!("../../fixtures/bert_page16_vector_chart_twocol.xml");
const BERT_FIGURE1_TOKEN_STRIP_XML: &str =
    include_str!("../../fixtures/bert_figure1_token_strip.xml");
#[cfg(unix)]
const BERT_MODEL_PARAMETER_FALSE_POSITIVE_XML: &str =
    include_str!("../../fixtures/bert_model_parameter_false_positive.xml");

/// PDF-10 synthetic fixtures: ja/zh CJK captions over chart labels.
const JA_VECTOR_FIGURES_XML: &str = include_str!("../../fixtures/ja_vector_figures.xml");
const ZH_VECTOR_FIGURES_XML: &str = include_str!("../../fixtures/zh_vector_figures.xml");

const MINIMAL_HELLO_XML: &str = r##"<pdf2xml>
  <page number="1" width="600" height="800">
    <fontspec id="0" size="12" family="Times" color="#000000"/>
    <text top="100" left="100" width="80" height="12" font="0">Hello PDF</text>
  </page>
</pdf2xml>"##;

const TEXT_ONLY_XML: &str = r##"<pdf2xml>
  <page number="1" width="600" height="800">
    <fontspec id="0" size="12" family="Times" color="#000000"/>
    <text top="100" left="100" width="180" height="12" font="0">Text only PDF.</text>
  </page>
</pdf2xml>"##;

const TEXT_ONLY_BASELINE: &str = "Text only PDF.\n";

const EMBED_IMAGE_XML: &str = r##"<pdf2xml>
  <page number="1" width="600" height="800">
    <fontspec id="0" size="14" family="Times" color="#000000"/>
    <fontspec id="1" size="12" family="Times" color="#000000"/>
    <text top="80" left="100" width="300" height="16" font="0">Paper Title</text>
    <image top="130" left="120" width="120" height="80" src="paper-1_1.png"/>
    <text top="218" left="120" width="260" height="12" font="1">Figure 1. A test image.</text>
    <text top="280" left="100" width="300" height="12" font="1">Body text after the figure.</text>
  </page>
</pdf2xml>"##;

const EMBED_IMAGE_BASELINE: &str =
    "Paper Title\nFigure 1. A test image.\nBody text after the figure.\n";

const TABLE_CROP_XML: &str = r##"<pdf2xml>
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
</pdf2xml>"##;

const TABLE_CROP_BASELINE: &str = "Body before table.\nTable 1. Scores.\nMetric 2019 2020\nA 0.91 0.93\nB 0.81 0.84\nBody after table.\n";

const DISPLAY_EQUATION_XML: &str = r##"<pdf2xml>
  <page number="1" width="600" height="800">
    <fontspec id="0" size="12" family="Times" color="#000000"/>
    <text top="80" left="100" width="260" height="12" font="0">Body before equation.</text>
    <text top="160" left="240" width="120" height="12" font="0">E = mc^2</text>
    <text top="230" left="100" width="260" height="12" font="0">Body after equation.</text>
  </page>
</pdf2xml>"##;

const DISPLAY_EQUATION_BASELINE: &str = "Body before equation.\nE = mc^2\nBody after equation.\n";

const LOWERCASE_CONTINUATION_XML: &str = r##"<pdf2xml>
  <page number="1" width="600" height="800">
    <fontspec id="0" size="12" family="Times" color="#000000"/>
    <text top="80" left="100" width="260" height="12" font="0">This paragraph</text>
    <image top="130" left="120" width="120" height="80" src="paper-1_1.png"/>
    <text top="260" left="100" width="260" height="12" font="0">continues after the figure.</text>
  </page>
</pdf2xml>"##;

const LOWERCASE_CONTINUATION_BASELINE: &str = "This paragraph\ncontinues after the figure.\n";

const INLINE_MATH_XML: &str = r##"<pdf2xml>
  <page number="1" width="600" height="800">
    <fontspec id="0" size="12" family="Times" color="#000000"/>
    <text top="100" left="100" width="360" height="12" font="0">The energy term E = mc^2 appears inline.</text>
  </page>
</pdf2xml>"##;

const INLINE_MATH_BASELINE: &str = "The energy term E = mc^2 appears inline.\n";

const TINY_PAGE_XML: &str = r##"<pdf2xml>
  <page number="1" width="600" height="800">
    <fontspec id="0" size="12" family="Times" color="#000000"/>
    <text top="100" left="100" width="20" height="12" font="0">Tiny</text>
  </page>
</pdf2xml>"##;

const TINY_PAGE_BASELINE: &str =
    "Tiny plus many baseline characters that the XML reconstruction did not recover.\n";

const ROTATED_AND_UNKNOWN_CAPTION_XML: &str = r##"<pdf2xml>
  <page number="1" width="600" height="800">
    <fontspec id="0" size="12" family="Times" color="#000000"/>
    <text top="100" left="100" width="360" height="12" font="0">Ordinary flowing prose continues here normally.</text>
    <text top="150" left="500" width="0" height="200" font="0">VERTICAL WATERMARK</text>
    <text top="400" left="100" width="300" height="12" font="0">चित्र ३: नतीजों की तुलना।</text>
  </page>
</pdf2xml>"##;

const ROTATED_AND_UNKNOWN_CAPTION_BASELINE: &str =
    "Ordinary flowing prose continues here normally.\nVERTICAL WATERMARK\nचित्र ३: नतीजों की तुलना।\n";

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

// ---------------------------------------------------------------------------
// Dual-path harness (TEST-2/PDF-2).
//
// Every migrated integration test below runs over BOTH implementations of
// [`PopplerBackend`]:
//
//   real-tool path — `cfg(unix)` only; poppler is stood in by /bin/sh
//     scripts, so the *actual* subprocess pipeline (spawn, env scrubbing,
//     pipes, exit statuses) executes end-to-end;
//   fake path — any OS; [`FakePoppler`] answers every backend surface
//     in-process with the same payloads/failure shapes and materializes
//     on-disk artifacts exactly as downstream code expects.
//
// Both paths consume the same shared document constants, so a divergence
// between "what poppler feeds us" and "what the fake feeds us" cannot
// hide. Tests whose extra value lies only in real-executable details are
// left single-path (unix); see the migration table in docs/report.md §4.5.

#[cfg(unix)]
fn shell_cat(body: &str, tag: &str) -> String {
    format!("#!/bin/sh\ncat <<'{tag}'\n{body}\n{tag}\n")
}

/// Which `pdfimages`/`pdftoppm` stand-ins a given case needs.
#[cfg(unix)]
enum UnixImageTools {
    /// Optional binaries absent (`pdfimages`/`pdftoppm` = `None`).
    Absent,
    /// Present and healthy; extraction yields no embedded images.
    HealthyEmpty,
    /// Extraction yields one image file with the given contents.
    OneRaster,
}

#[cfg(unix)]
fn unix_tools(root: &Path, doc: &str, baseline: &str, images: UnixImageTools) -> PopplerTools {
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("bin dir");
    let pdftohtml = bin.join("pdftohtml");
    write_executable(&pdftohtml, &shell_cat(doc, "XMLDOC"));
    let pdftotext = bin.join("pdftotext");
    write_executable(&pdftotext, &shell_cat(baseline, "TEXTBASE"));
    match images {
        UnixImageTools::Absent => PopplerTools {
            pdftohtml,
            pdftotext,
            pdfimages: None,
            pdftoppm: None,
        },
        UnixImageTools::HealthyEmpty | UnixImageTools::OneRaster => {
            let pdfimages = bin.join("pdfimages");
            match images {
                UnixImageTools::OneRaster => fake_pdfimages(&pdfimages),
                _ => fake_pdfimages_empty(&pdfimages),
            }
            let pdftoppm = bin.join("pdftoppm");
            fake_pdftoppm(&pdftoppm);
            PopplerTools {
                pdftohtml,
                pdftotext,
                pdfimages: Some(pdfimages),
                pdftoppm: Some(pdftoppm),
            }
        }
    }
}

fn write_input_pdf(dir: &Path) -> PathBuf {
    let input = dir.join("input.pdf");
    fs::write(&input, b"dummy pdf").expect("input pdf fixture");
    input
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

#[test]
fn pdftohtml_timeout_aborts_conversion_before_any_baseline_work() {
    use crate::tools::ToolError;

    // Timing failure injected through the in-process fake: the pipeline
    // must surface ToolError::TimedOut, write nothing, and never touch
    // the remaining surfaces (call counters prove the abort point).
    let dir = tempfile::tempdir().expect("fake-path temp dir");
    let input = write_input_pdf(dir.path());
    let output = dir.path().join("output.epub");
    let fake = FakePoppler::new(MINIMAL_HELLO_XML, "baseline would exist")
        .timing_out_xml(crate::tools::DEFAULT_POPPLER_TIMEOUT);

    let result = convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), &fake, None);

    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("timed-out XML extraction aborts"),
    };
    assert!(
        matches!(
            &error,
            crate::PdfError::Tool(ToolError::TimedOut {
                tool: "pdftohtml",
                ..
            })
        ),
        "unexpected error: {error}"
    );
    assert!(!output.exists());
    assert_eq!(fake.call_count("pdf_to_xml"), 1);
    assert_eq!(fake.call_count("pdf_to_text"), 0);
    assert_eq!(fake.call_count("extract_images"), 0);
}

#[test]
fn raster_render_failure_skips_media_crops_with_warnings_and_keeps_text() {
    use std::io::Read;
    use zip::ZipArchive;

    let dir = tempfile::tempdir().expect("fake-path temp dir");
    let input = write_input_pdf(dir.path());
    let output = dir.path().join("output.epub");
    let fake = FakePoppler::new(TABLE_CROP_XML, TABLE_CROP_BASELINE)
        .with_render_failure("pdftoppm exploded");

    let outcome = convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), &fake, None)
        .expect("conversion degrades gracefully when rendering fails");

    assert_eq!(outcome.report.tables, 0);
    assert_eq!(outcome.report.figures, 0);
    assert!(
        outcome
            .report
            .warnings
            .iter()
            .any(|warning| warning.contains("skipped table crop")
                && warning.contains("raster rendering failed")),
        "{:?}",
        outcome.report.warnings
    );
    // Nothing was cropped away, so the whole page stays translatable text.
    let mut archive =
        ZipArchive::new(fs::File::open(&output).expect("epub opens")).expect("zip opens");
    let mut content = String::new();
    archive
        .by_name("content.xhtml")
        .expect("content exists")
        .read_to_string(&mut content)
        .expect("content reads");
    assert!(content.contains("0.91"));
    assert_eq!(
        fake.call_count("render_page"),
        1,
        "page renders are cached per crop pass"
    );
}

#[test]
fn convert_pdf_does_not_write_epub_when_baseline_fails() {
    fn case(dir: &Path, tools: &dyn PopplerBackend) {
        let input = write_input_pdf(dir);
        let output = dir.join("output.epub");

        let result =
            convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), tools, None);

        assert!(result.is_err());
        assert!(
            !output.exists(),
            "output EPUB should not be written after baseline failure"
        );
    }

    #[cfg(unix)]
    {
        // Real-tool path: pdftotext stand-in exits 9 with a stderr note.
        let root = tempfile::tempdir().expect("real-path temp dir");
        let tools = unix_tools(
            root.path(),
            MINIMAL_HELLO_XML,
            "unused baseline",
            UnixImageTools::Absent,
        );
        let pdftotext = tools.pdftotext.clone();
        write_executable(
            &pdftotext,
            r#"#!/bin/sh
echo baseline failed >&2
exit 9
"#,
        );
        let input = write_input_pdf(root.path());
        let output = root.path().join("output.epub");
        assert!(
            convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), &tools, None)
                .is_err()
        );
        assert!(!output.exists());
    }

    // In-process path: identical pdftotext failure, zero processes.
    let fake_dir = tempfile::tempdir().expect("fake-path temp dir");
    let fake = FakePoppler::new(MINIMAL_HELLO_XML, "unused baseline")
        .failing_baseline(9, "baseline failed");
    case(fake_dir.path(), &fake);
}

#[test]
fn convert_pdf_continues_text_only_without_optional_image_tools() {
    use std::io::Read;
    use zip::ZipArchive;

    fn case(dir: &Path, tools: &dyn PopplerBackend) {
        let input = write_input_pdf(dir);
        let output = dir.join("output.epub");

        let outcome =
            convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), tools, None)
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
    {
        let root = tempfile::tempdir().expect("real-path temp dir");
        let tools = unix_tools(
            root.path(),
            TEXT_ONLY_XML,
            TEXT_ONLY_BASELINE,
            UnixImageTools::Absent,
        );
        case(root.path(), &tools);
    }

    let fake_dir = tempfile::tempdir().expect("fake-path temp dir");
    let fake = FakePoppler::new(TEXT_ONLY_XML, TEXT_ONLY_BASELINE).without_image_tool();
    case(fake_dir.path(), &fake);
}

#[test]
fn convert_pdf_embeds_extracted_image_with_translatable_caption() {
    use std::io::Read;
    use zip::ZipArchive;

    fn case(dir: &Path, tools: &dyn PopplerBackend) {
        let input = write_input_pdf(dir);
        let output = dir.join("output.epub");

        let outcome =
            convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), tools, None)
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
    {
        let root = tempfile::tempdir().expect("real-path temp dir");
        let tools = unix_tools(
            root.path(),
            EMBED_IMAGE_XML,
            EMBED_IMAGE_BASELINE,
            UnixImageTools::OneRaster,
        );
        case(root.path(), &tools);
    }

    let fake_dir = tempfile::tempdir().expect("fake-path temp dir");
    let fake = FakePoppler::new(EMBED_IMAGE_XML, EMBED_IMAGE_BASELINE)
        .with_extracted_image(FixtureImage::with_bytes(1, 120, 80, b"fake-image"));
    case(fake_dir.path(), &fake);
}

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

#[test]
fn convert_pdf_preserves_vector_chart_fixture_as_captioned_crop() {
    use std::io::Read;
    use zip::ZipArchive;

    const BASELINE: &str = "Vector-chart results are below.\n1.0 0.5 0.0 0 10 20 Epoch\nFigure 5. Vector chart of held-out accuracy.\nThe next paragraph should stay prose.\n";

    fn case(dir: &Path, tools: &dyn PopplerBackend) {
        let input = write_input_pdf(dir);
        let output = dir.join("output.epub");

        let outcome =
            convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), tools, None)
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
    {
        let root = tempfile::tempdir().expect("real-path temp dir");
        let tools = unix_tools(
            root.path(),
            BERT_FIGURE5_VECTOR_CHART_XML,
            BASELINE,
            UnixImageTools::HealthyEmpty,
        );
        case(root.path(), &tools);
    }

    let fake_dir = tempfile::tempdir().expect("fake-path temp dir");
    let fake = FakePoppler::new(BERT_FIGURE5_VECTOR_CHART_XML, BASELINE);
    case(fake_dir.path(), &fake);
}

#[test]
fn convert_pdf_warns_on_lowercase_continuation_after_media() {
    fn case(dir: &Path, tools: &dyn PopplerBackend) {
        let input = write_input_pdf(dir);
        let output = dir.join("output.epub");

        let outcome =
            convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), tools, None)
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
    {
        let root = tempfile::tempdir().expect("real-path temp dir");
        let tools = unix_tools(
            root.path(),
            LOWERCASE_CONTINUATION_XML,
            LOWERCASE_CONTINUATION_BASELINE,
            UnixImageTools::OneRaster,
        );
        case(root.path(), &tools);
    }

    let fake_dir = tempfile::tempdir().expect("fake-path temp dir");
    let fake = FakePoppler::new(LOWERCASE_CONTINUATION_XML, LOWERCASE_CONTINUATION_BASELINE)
        .with_extracted_image(FixtureImage::with_bytes(1, 120, 80, b"fake-image"));
    case(fake_dir.path(), &fake);
}

#[test]
fn convert_pdf_preserves_detected_table_as_crop_with_caption() {
    use std::io::Read;
    use zip::ZipArchive;

    fn case(dir: &Path, tools: &dyn PopplerBackend) {
        let input = write_input_pdf(dir);
        let output = dir.join("output.epub");

        let outcome =
            convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), tools, None)
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
    {
        let root = tempfile::tempdir().expect("real-path temp dir");
        let tools = unix_tools(
            root.path(),
            TABLE_CROP_XML,
            TABLE_CROP_BASELINE,
            UnixImageTools::HealthyEmpty,
        );
        case(root.path(), &tools);
    }

    let fake_dir = tempfile::tempdir().expect("fake-path temp dir");
    let fake = FakePoppler::new(TABLE_CROP_XML, TABLE_CROP_BASELINE);
    case(fake_dir.path(), &fake);
}

#[test]
fn convert_pdf_preserves_display_equation_as_crop() {
    use std::io::Read;
    use zip::ZipArchive;

    fn case(dir: &Path, tools: &dyn PopplerBackend) {
        let input = write_input_pdf(dir);
        let output = dir.join("output.epub");

        let outcome =
            convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), tools, None)
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
    {
        let root = tempfile::tempdir().expect("real-path temp dir");
        let tools = unix_tools(
            root.path(),
            DISPLAY_EQUATION_XML,
            DISPLAY_EQUATION_BASELINE,
            UnixImageTools::HealthyEmpty,
        );
        case(root.path(), &tools);
    }

    let fake_dir = tempfile::tempdir().expect("fake-path temp dir");
    let fake = FakePoppler::new(DISPLAY_EQUATION_XML, DISPLAY_EQUATION_BASELINE);
    case(fake_dir.path(), &fake);
}

#[test]
fn convert_pdf_marks_inline_math_as_protected_span() {
    fn case(dir: &Path, tools: &dyn PopplerBackend) {
        let input = write_input_pdf(dir);
        let output = dir.join("output.epub");

        convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), tools, None)
            .expect("conversion should succeed");

        let book = bookforge_epub::read_epub(&output).expect("converted EPUB should be readable");
        assert!(
            book.blocks.iter().any(|block| {
                block.protected_spans.iter().any(|span| {
                    span.kind == bookforge_core::ir::ProtectedSpanKind::Math
                        && span.text == "E = mc^2"
                })
            }),
            "inline math should become a protected span after PDF conversion"
        );
    }

    #[cfg(unix)]
    {
        let root = tempfile::tempdir().expect("real-path temp dir");
        let tools = unix_tools(
            root.path(),
            INLINE_MATH_XML,
            INLINE_MATH_BASELINE,
            UnixImageTools::HealthyEmpty,
        );
        case(root.path(), &tools);
    }

    let fake_dir = tempfile::tempdir().expect("fake-path temp dir");
    let fake = FakePoppler::new(INLINE_MATH_XML, INLINE_MATH_BASELINE);
    case(fake_dir.path(), &fake);
}

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

#[test]
fn low_confidence_pages_linearize_by_default() {
    use std::io::Read;
    use zip::ZipArchive;

    fn case(dir: &Path, tools: &dyn PopplerBackend) {
        let input = write_input_pdf(dir);
        let output = dir.join("output.epub");

        let outcome =
            convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), tools, None)
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
    {
        let root = tempfile::tempdir().expect("real-path temp dir");
        let tools = unix_tools(
            root.path(),
            TINY_PAGE_XML,
            TINY_PAGE_BASELINE,
            UnixImageTools::HealthyEmpty,
        );
        case(root.path(), &tools);
    }

    let fake_dir = tempfile::tempdir().expect("fake-path temp dir");
    let fake = FakePoppler::new(TINY_PAGE_XML, TINY_PAGE_BASELINE);
    case(fake_dir.path(), &fake);
}

#[test]
fn low_confidence_pages_can_be_preserved_as_page_images() {
    use std::io::Read;
    use zip::ZipArchive;

    fn case(dir: &Path, tools: &dyn PopplerBackend) {
        let input = write_input_pdf(dir);
        let output = dir.join("output.epub");

        let options = ConvertOptions {
            low_confidence: LowConfidenceMode::Preserve,
            ..ConvertOptions::default()
        };
        let outcome = convert_pdf_with_tools(&input, &output, &options, tools, None)
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

    #[cfg(unix)]
    {
        let root = tempfile::tempdir().expect("real-path temp dir");
        let tools = unix_tools(
            root.path(),
            TINY_PAGE_XML,
            TINY_PAGE_BASELINE,
            UnixImageTools::HealthyEmpty,
        );
        case(root.path(), &tools);
    }

    let fake_dir = tempfile::tempdir().expect("fake-path temp dir");
    let fake = FakePoppler::new(TINY_PAGE_XML, TINY_PAGE_BASELINE);
    case(fake_dir.path(), &fake);
}

#[test]
fn unrecognized_numeral_systems_are_the_only_skipped_caption_residue() {
    // PDF-10 deep half: after English, CJK prefixes and Latin/fullwidth
    // digit shapes became POSITIVE detections, only captions whose
    // ordinals use unhandled numeral systems (Devanagari, Thai) remain
    // "skipped" — the wave-2 warning narrows to genuinely unknown scripts.
    let positives = [
        "चित्र ३: परिणामों की तुलना।",
        "รูปที่ ๓: ผลการทดลองทั้งหมด",
        // Arabic-Indic ordinals sit outside the Latin/fullwidth
        // repertoire the detector acts on, so they stay unhandled too.
        "شكل ١٢: مقارنة النماذج",
    ];
    for text in positives {
        assert!(
            detection::localized_caption_skipped(text),
            "non-Latin ordinals stay unhandled and must be counted: {text}"
        );
    }
    // Everything below is now DETECTED, so it must not be warned about.
    let negatives = [
        "Figure 1. English caption handled normally.",
        "Table 2. Also handled.",
        "Abbildung 3: Verteilung der Genauigkeit.",
        "Tabla 2: Resultados del experimento.",
        "Figura 12 – Comparativo de modelos",
        "図1 学習曲線の比較",
        "図 12：推定結果",
        "図１（全文）を示す",
        "图１ 全局精度对比",
        "圖7 系統架構示意",
        "表5 実験結果の一覧",
        "表 5：測定値のまとめ",
        "그림 1: 모델 구조 비교",
        "Afbeelding 4 – Geminieten waarden",
        "1. Introduction",
        "In 2019: the study was replicated with more participants than ever before recorded.",
        "The results were significant across all conditions tested by the authors of the paper.",
    ];
    for text in negatives {
        assert!(
            !detection::localized_caption_skipped(text),
            "covered repertoire must not warn: {text}"
        );
    }

    let mut page = empty_page(1);
    page.fragments = vec![fragment(300, 100, 240, 12, "चित्र ३: परिणामों की तुलना।")];
    let pages = vec![page];
    let warning = detection::skipped_foreign_caption_warning(&pages).expect("warning");
    assert!(warning.contains('1'), "{warning}");
    assert!(warning.contains("does not recognize"), "{warning}");

    assert!(detection::skipped_foreign_caption_warning(&[empty_page(1)]).is_none());
}

#[test]
fn cjk_caption_prefixes_take_ascii_or_fullwidth_ordinals() {
    // Tier 2 of the detector: typed CJK prefixes; Japanese figure, both
    // Chinese figure variants, shared table prefix, fullwidth digits.
    for text in [
        "図1 学習曲線の比較",
        "図 12：推定誤差の分解",
        "図３ 推定手法ごとの比較", // fullwidth ３ directly attached
        "図\u{3000}10 全体構成",   // ideographic space
        "图１ 全局精度对比",       // simplified + fullwidth ordinal
        "圖7 系統架構示意",        // traditional + ASCII ordinal
        "表5 実験結果の一覧",
        "表 5：測定値のまとめ",
        "表５ 灰色予測モデル誤差", // table + fullwidth
    ] {
        assert!(
            is_figure_caption_text(text) || is_table_caption_text(text),
            "must classify as a caption: {text}"
        );
    }
    for (text, figure_expected) in [
        ("図1 学習曲線の比較", true),
        ("图３ 架构对比", true),
        ("圖2 通訊流程圖", true),
        ("表5 実験結果の一覧", false),
        ("表４ 三種介入的效果", false),
    ] {
        assert!(
            is_figure_caption_text(text) || is_table_caption_text(text),
            "expected caption classification: {text}"
        );
        assert_eq!(
            is_figure_caption_text(text),
            figure_expected,
            "wrong tier assignment: {text}"
        );
    }
    // CJK characters WITHOUT an ordinal are ordinary prose starts, not
    // captions.
    for text in [
        "表現力の高い新規性项",
        "図は研究開発費と生産性との関係を示している。",
        "表紙に戻る読者は少なくない。",
        "第一章　序論",
    ] {
        assert!(
            !is_figure_caption_text(text) && !is_table_caption_text(text),
            "prose-like CJK lead must not classify: {text}"
        );
    }
}

#[test]
fn language_neutral_fallback_detects_localized_captions_without_prefix_tables() {
    // Tier 3: short alphabetic leading word in ANY script + explicit
    // Latin/fullwidth ordinal + separator. These previously only fed the
    // skipped-caption warning; they now associate with figures/tables.
    for text in [
        "Abbildung 3: Verteilung der Genauigkeit.", // German
        "Tabla 2: Resultados del experimento.",     // Spanish
        "Figura 12 – Comparativo de modelos",       // Portuguese, spaced dash
        "그림 1: 모델 구조 비교.",                  // Korean, no prefix table entry
        "Абзацей 5: вынікі эксперыментаў.",         // Cyrillic script
        "Ανικόυρε 6. Σύνολο μοντέλων",              // Greek-script lead (synthetic)
        "Tafel 8: gemiddelde prestasie.",           // Afrikaans table lead via fallback
    ] {
        assert!(
            is_figure_caption_text(text) || is_table_caption_text(text),
            "fallback must detect the caption shape: {text}"
        );
    }

    // Prose guards survive: numbered headings, long sentences, year-led
    // sentences without a leading word+ordinal+separator shape.
    for text in [
        "1. Introduction",
        "In 2019: the study was replicated with more participants than ever before recorded.",
        "A longer sentence with many words that happens to contain 42 values but never ends with a proper separator pattern early enough to look like one.",
    ] {
        assert!(
            !is_figure_caption_text(text) && !is_table_caption_text(text),
            "must not classify prose/heading as caption: {text}"
        );
    }

    // The fallback feeds vector-figure association end to end: give it a
    // caption plus chart labels and confirm a region is recovered.
    let mut page = empty_page(1);
    page.width = 600;
    page.height = 800;
    page.fragments = vec![
        fragment(100, 250, 24, 10, "80"),
        fragment(140, 252, 22, 10, "60"),
        fragment(180, 252, 22, 10, "40"),
        fragment(220, 252, 22, 10, "20"),
        fragment(330, 180, 240, 15, "Tabella 9 – Rendimento dei modelli."),
    ];
    let mut warnings = Vec::new();
    let single_page = [page];
    let unmarked_columns = HashSet::new();
    let regions = vector_figure_regions(&single_page, &unmarked_columns, &mut warnings);

    assert_eq!(
        regions.len(),
        1,
        "Italian-tab-shaped caption recovers a region: {warnings:?}"
    );
}

#[test]
fn converted_japanese_and_chinese_vector_figures_associate_via_cjk_captions() {
    use std::io::Read;
    use zip::ZipArchive;

    fn case(dir: &Path, tools: &dyn PopplerBackend, expected_caption: &str) {
        let input = write_input_pdf(dir);
        let output = dir.join("output.epub");

        let outcome =
            convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), tools, None)
                .expect("conversion should succeed");

        assert_eq!(outcome.report.images, 0);
        assert_eq!(outcome.report.figures, 1, "{}", outcome.report.summary());
        assert!(
            !outcome
                .report
                .warnings
                .iter()
                .any(|warning| warning.contains("caption detector")),
            "CJK captions are detected now; nothing may be reported as skipped: {:?}",
            outcome.report.warnings
        );

        let mut archive =
            ZipArchive::new(fs::File::open(&output).expect("epub opens")).expect("zip opens");
        let mut content = String::new();
        archive
            .by_name("content.xhtml")
            .expect("content exists")
            .read_to_string(&mut content)
            .expect("content reads");
        assert!(
            content.contains("<figure id=\"pdf-figure-0001\">"),
            "vector chart becomes one figure crop: {content}"
        );
        assert_eq!(content.matches(expected_caption).count(), 1, "{content}");
        assert!(
            content.contains(&format!("<figcaption>{expected_caption}</figcaption>")),
            "caption is carried on the figure: {content}"
        );
        // Chart label rows must have been consumed into the raster crop
        // (credited as media), not left behind as scattered paragraphs.
        assert!(
            outcome.report.media_preserved_chars > 0,
            "axis labels leave the text flow: {:?}",
            outcome.report.summary()
        );
    }

    const JA_BASELINE: &str = "80\n60\n40\n20\n0\nエポック数\n図1 学習曲線の比較\n";
    const ZH_BASELINE: &str = "95\n90\n85\n80\n75\n训练轮次\n图１ 全局精度对比\n";

    for (doc, baseline, caption) in [
        (JA_VECTOR_FIGURES_XML, JA_BASELINE, "図1 学習曲線の比較"),
        (ZH_VECTOR_FIGURES_XML, ZH_BASELINE, "图１ 全局精度对比"),
    ] {
        #[cfg(unix)]
        {
            let root = tempfile::tempdir().expect("real-path temp dir");
            let tools = unix_tools(root.path(), doc, baseline, UnixImageTools::HealthyEmpty);
            case(root.path(), &tools, caption);
        }

        let fake_dir = tempfile::tempdir().expect("fake-path temp dir");
        let fake = FakePoppler::new(doc, baseline);
        case(fake_dir.path(), &fake, caption);
    }
}

#[test]
fn running_header_removal_does_not_push_pages_below_the_confidence_threshold() {
    // PDF-6: the 95% threshold judges pre-header-removal coverage.
    let mut credited_only_headers = PageStats {
        page: 3,
        lines: 4,
        chars: 60,
        baseline_chars: 100,
        running_header_chars: 35,
        two_column: false,
        rtl_dominant: false,
        low_confidence: false,
        low_confidence_action: None,
    };
    mark_low_confidence_pages(
        std::slice::from_mut(&mut credited_only_headers),
        LowConfidenceMode::Linearize,
    );
    assert!(
        !credited_only_headers.low_confidence,
        "deficit explained by removed headers must not rasterize the page"
    );

    let mut genuinely_missing = PageStats {
        page: 4,
        lines: 4,
        chars: 40,
        baseline_chars: 100,
        running_header_chars: 0,
        two_column: false,
        rtl_dominant: false,
        low_confidence: false,
        low_confidence_action: None,
    };
    mark_low_confidence_pages(
        std::slice::from_mut(&mut genuinely_missing),
        LowConfidenceMode::Linearize,
    );
    assert!(
        genuinely_missing.low_confidence,
        "real deficits must still flag"
    );
}

#[test]
fn ocr_render_dpi_caps_extreme_media_boxes() {
    // PDF-22: A4-ish pages keep full DPI; a billboard MediaBox is
    // downscaled under the pixel budget; degenerate inputs clamp.
    let a4 = max_ocr_render_dpi(595.0, 842.0, MAX_OCR_RENDER_PIXELS);
    assert_eq!(
        a4, PDF_RENDER_DPI,
        "ordinary pages stay at {PDF_RENDER_DPI} DPI"
    );

    let extreme = max_ocr_render_dpi(20_000.0, 40_000.0, MAX_OCR_RENDER_PIXELS);
    let pixels = (20_000.0 * f64::from(extreme) / 72.0) * (40_000.0 * f64::from(extreme) / 72.0);
    assert!(
        pixels <= MAX_OCR_RENDER_PIXELS as f64,
        "extreme MediaBox must fit the budget: {pixels}"
    );

    assert_eq!(
        max_ocr_render_dpi(0.0, 0.0, MAX_OCR_RENDER_PIXELS),
        PDF_RENDER_DPI,
        "degenerate dimensions cannot over-constrain"
    );
}

#[test]
fn failed_figure_pass_still_removes_scratch_temp_dirs() {
    // PDF-3 regression companion to the RAII drop test in tools.rs: the
    // dangling-image fixture makes `image_asset`'s fs::read fail inside
    // figure_blocks_from_images so the error escapes before any manual
    // cleanup line could ever run. With ScopedTempDir guards owning the
    // directories on the convert stack frame, unwinding cannot leak.
    fn case(dir: &Path, tools: &dyn PopplerBackend) {
        let input = write_input_pdf(dir);
        let output = dir.join("output.epub");

        let result =
            convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), tools, None);

        assert!(result.is_err(), "figure pass must fail for this fixture");
        assert!(!output.exists(), "no EPUB is produced when a pass errors");

        // And the RAII guarantee those guards provide:
        let guard = crate::tools::ScopedTempDir::new("bookforge-pdf-probe").expect("probe dir");
        let probe_path = guard.path().to_path_buf();
        assert!(probe_path.is_dir());
        drop(guard);
        assert!(
            !probe_path.exists(),
            "a dropped scoped scratch dir must be gone even when owners return early"
        );
    }

    const DANGLING_IMAGE_XML: &str = r##"<pdf2xml>
  <page number="1" width="600" height="800">
    <fontspec id="0" size="12" family="Times" color="#000000"/>
    <text top="80" left="100" width="300" height="16" font="0">Paper Title</text>
    <image top="130" left="120" width="120" height="80" src="paper-1_1.png"/>
    <text top="218" left="120" width="260" height="12" font="0">Figure 1. A test image.</text>
  </page>
</pdf2xml>"##;
    const DANGLING_IMAGE_BASELINE: &str = "Paper Title\nFigure 1. A test image.\n";

    #[cfg(unix)]
    {
        // Real-tool path: pdfimages lists one healthy image but never
        // materializes its file (`write_executable_orphan`).
        let root = tempfile::tempdir().expect("real-path temp dir");
        let bin = root.path().join("bin");
        fs::create_dir_all(&bin).expect("bin dir");
        let pdftohtml = bin.join("pdftohtml");
        write_executable(&pdftohtml, &shell_cat(DANGLING_IMAGE_XML, "XMLDOC"));
        let pdftotext = bin.join("pdftotext");
        write_executable(&pdftotext, &shell_cat(DANGLING_IMAGE_BASELINE, "TEXTBASE"));
        let pdfimages = write_executable_orphan(&bin, "pdfimages");
        let pdftoppm = bin.join("pdftoppm");
        fake_pdftoppm(&pdftoppm);
        let tools = PopplerTools {
            pdftohtml,
            pdftotext,
            pdfimages: Some(pdfimages),
            pdftoppm: Some(pdftoppm),
        };
        case(root.path(), &tools);
    }

    // In-process path: an advertised-but-unwritten extraction artifact
    // produces the identical dangling path and failure.
    let fake_dir = tempfile::tempdir().expect("fake-path temp dir");
    let fake = FakePoppler::new(DANGLING_IMAGE_XML, DANGLING_IMAGE_BASELINE)
        .with_extracted_image(FixtureImage::dangling(1, 640, 480));
    case(fake_dir.path(), &fake);
}

#[cfg(unix)]
fn write_executable_orphan(dir: &Path, name: &str) -> PathBuf {
    // `pdfimages` that lists one image but never creates its file.
    let path = dir.join(name);
    write_executable(
        &path,
        r#"#!/bin/sh
if [ "$1" = "-list" ]; then
cat <<'LIST'
page   num  type   width height color comp bpc  enc interp  object ID x-ppi y-ppi size ratio
--------------------------------------------------------------------------------------------
   1     0 image     640   480  rgb     3   8  image  no        12  0    72    72  1K  1.0%
LIST
exit 0
fi
for last do :; done
# Advertise the output path without ever writing it.
printf '%s-000-000.png\n' "$last"
"#,
    );
    path
}

#[test]
fn rotated_and_unknown_script_captions_surface_in_report_warnings() {
    // PDF-10, narrowed-warning half: since detection now covers English,
    // CJK prefixes and any Latin/fullwidth-digit caption shape, the report
    // notice fires ONLY for captions whose ordinals use genuinely
    // unhandled numeral systems (Devanagari here). The rotated fragment
    // assertion is unchanged.
    fn case(dir: &Path, tools: &dyn PopplerBackend) {
        let input = write_input_pdf(dir);
        let output = dir.join("output.epub");

        let outcome =
            convert_pdf_with_tools(&input, &output, &ConvertOptions::default(), tools, None)
                .expect("conversion should succeed");

        assert!(
            outcome
                .report
                .warnings
                .iter()
                .any(|warning| warning.contains("rotated/zero-width text fragment")),
            "vertical text must be reported, not silently dropped: {:?}",
            outcome.report.summary()
        );
        assert!(
            outcome
                .report
                .warnings
                .iter()
                .any(|warning| warning.contains("caption detector does not recognize")),
            "unknown-script captions must still be counted in the report: {:?}",
            outcome.report.warnings
        );
    }

    #[cfg(unix)]
    {
        let root = tempfile::tempdir().expect("real-path temp dir");
        let tools = unix_tools(
            root.path(),
            ROTATED_AND_UNKNOWN_CAPTION_XML,
            ROTATED_AND_UNKNOWN_CAPTION_BASELINE,
            UnixImageTools::HealthyEmpty,
        );
        case(root.path(), &tools);
    }

    let fake_dir = tempfile::tempdir().expect("fake-path temp dir");
    let fake = FakePoppler::new(
        ROTATED_AND_UNKNOWN_CAPTION_XML,
        ROTATED_AND_UNKNOWN_CAPTION_BASELINE,
    );
    case(fake_dir.path(), &fake);
}

#[test]
fn invisible_formatting_marks_do_not_skew_rtl_page_coverage() {
    // PDF-7, coverage-metric half: poppler's shapers leak zero-width
    // formatting controls (ZWNJ/ZWJ/RLM…) asymmetrically between
    // pdftotext and pdftohtml -xml. Counting them on either side made
    // RTL pages swing under the 95% threshold and spend OCR purely for
    // their script. Both sides of the ratio now weigh the same visible
    // repertoire.
    let visible = "المنظومة القياسية الدولية للوحدات تعرف بسبع وحدات اساس.";
    let mut baseline = String::with_capacity(visible.len() * 2);
    for ch in visible.chars() {
        baseline.push(ch);
        if !ch.is_whitespace() {
            baseline.push('\u{200c}'); // ZWNJ between every letter
        }
    }

    let xml = format!(
        r#"<pdf2xml>
<page number="1" width="600" height="800">
<fontspec id="0" size="12" family="T"/>
<text top="100" left="80" width="440" height="14" font="0">{visible}</text>
</page>
</pdf2xml>"#
    );
    let pages = parse_pdf2xml(&xml).expect("fixture parses");

    let reconstruction = reconstruct_with_chapter_guard(&pages, ColumnMode::Single, None);
    let reconstructed_chars = reconstruction
        .blocks
        .iter()
        .map(|anchored| anchored.block.char_count())
        .sum::<usize>();
    assert_eq!(
        reconstructed_chars,
        crate::model::count_visible_chars(visible),
        "reconstruction weighs visible characters only"
    );

    // Naive char counting would have seen roughly half of the baseline
    // missing; the symmetric metric sees full coverage.
    let naive_baseline = baseline.chars().filter(|ch| !ch.is_whitespace()).count();
    // The pre-fix asymmetry was material — nearly half of the naive
    // baseline counted invisible controls.
    assert!(
        naive_baseline >= reconstructed_chars + 20,
        "fixture must exhibit real drift: {naive_baseline} vs {reconstructed_chars}"
    );

    let mut page_stats = reconstruction.pages;
    assert_eq!(page_stats.len(), 1);
    assert!(
        page_stats[0].rtl_dominant,
        "the fixture is genuinely RTL-dominant"
    );
    page_stats[0].baseline_chars = crate::model::count_visible_chars(&baseline);
    mark_low_confidence_pages(&mut page_stats, LowConfidenceMode::Linearize);
    assert!(
        !page_stats[0].low_confidence,
        "formatting controls must not rasterize/OCR an RTL page: {:?}",
        page_stats[0]
    );
}
