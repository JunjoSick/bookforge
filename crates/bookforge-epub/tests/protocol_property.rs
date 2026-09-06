//! Roundtrip property harness for the reader↔writer marker protocol
//! (audit investment #4 / EPUB-18).
//!
//! Deterministic pseudo-random XHTML bodies are composed from a grammar of
//! protocol-sensitive building blocks and driven through the full pipeline
//! (`read_epub` → identity-plus-prefix translations → `rebuild_epub`).
//! Every generated fixture asserts:
//!
//! 1. **Raw untouched-byte stability** — spine documents receiving no
//!    translations are copied byte-for-byte (entries compared as decompressed
//!    bytes), and inside translated documents every artifact that must remain
//!    raw survives exactly: suppressed `script`/`style`/`svg`/`math`
//!    subtrees, comments, and protected-span wrappers.
//! 2. **Marker/protected-span survival under translate-replace** — every
//!    required inline marker and protected span comes back; graceful writer
//!    skips surface as missing translation prefixes.
//! 3. **Single-stream shared ordinal** — reader-side paired (`m`) and empty
//!    (`r`) marker ids draw from ONE counter, unique and consecutive per
//!    block (the wave-1 regression class).
//! 4. **Determinism** — rebuilding twice produces identical archive bytes.
//!
//! SEED RANGE DOCUMENTATION (offline reproducibility, no runtime entropy):
//! batches cover corpus seeds `0..=199`; each seed is a plain integer index
//! mixed into the PRNG with fixed constants. Batch layout:
//!   * `property_batch_seeds_000_049` → seeds   0–49
//!   * `property_batch_seeds_050_099` → seeds  50–99
//!   * `property_batch_seeds_100_149` → seeds 100–149
//!   * `property_batch_seeds_150_199` → seeds 150–199
//!
//! Every failure message embeds `seed=N` so an offending fixture is
//! reproducible from the literal seed alone.
//!
//! PROTOCOL NOTE (assertion scoping): translate-replace owns the whole
//! interior of a translated block, so text AND comments nested inside such
//! blocks are legitimately folded into the re-rendered translation; the
//! grammar therefore injects comments only at body level, where they must
//! survive byte-for-byte (asserted). Suppressed-subtree interiors likewise
//! stay out of the translatable payload (asserted at IR level) while their
//! bytes remain verbatim inside the rebuilt document (slices asserted).

use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{Read as _, Write as _},
    path::Path,
    time::{Duration, Instant},
};

use bookforge_core::{
    config::{BilingualMode, SegmentationConfig},
    ir::Block,
    marker::marker_ids_in_text,
    segment::{BlockTranslation, build_segments},
};
use bookforge_epub::{RebuildOptions, inspect_epub, read_epub, rebuild_epub_with_options};
use quick_xml::{
    Reader,
    events::{BytesStart, Event},
};
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

/// Corpus seeds are the integers `0..=199`; the PRNG state is mixed with
/// fixed constants (multiplier/offset) to decorrelate neighbouring seeds.
const SEED_MULTIPLIER: u64 = 2654435761;
const SEED_OFFSET: u64 = 12345;

/// Translation-prefix sentinel, prepended to every supplied translation so
/// applied replacements are observable at the XHTML level: a graceful skip
/// by the writer drops the prefix for exactly that block.
const APPLIED_MARK: &str = "[Tr]";

// ---------------------------------------------------------------------------
// Deterministic PRNG: seeded xorshift64*, no runtime entropy anywhere.
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // Zero would degenerate xorshift; mixing guarantees a nonzero state.
        let mixed = seed
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(0xD1B5_4A32_D192_ED03);
        Rng(if mixed == 0 { 1 } else { mixed })
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next_u64() % bound as u64) as usize
        }
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

// ---------------------------------------------------------------------------
// Generated-document bookkeeping
// ---------------------------------------------------------------------------

#[derive(Default)]
struct GenDoc {
    body: String,
    /// Atomic tokens living in translatable text; must survive (decoded).
    visible_sentinels: Vec<String>,
    /// Tokens living inside suppressed subtrees; must never leak into
    /// translatable text, while their raw slice survives verbatim.
    hidden_sentinels: Vec<String>,
    /// Exact comment substrings that must survive byte-for-byte.
    comments: Vec<String>,
    /// CDATA payload text that must survive semantically (decoded level).
    /// The CDATA wrapper form itself is deliberately not pinned: replace
    /// mode legitimately re-renders block text as escaped character data.
    cdata_payloads: Vec<String>,
    /// Exact suppressed-subtree slices that must survive byte-for-byte.
    suppressed_slices: Vec<String>,
    /// Exact protected-span wrapper slices that must survive byte-for-byte.
    guard_slices: Vec<String>,
    /// Tokens detected as protected spans; must survive (decoded level).
    protected_tokens: Vec<String>,
}

/// Serial counter shared by all token producers of one document.
#[derive(Default)]
struct GenState {
    serial: u64,
}

impl GenState {
    fn next_serial(&mut self) -> u64 {
        self.serial += 1;
        self.serial
    }
}

fn push_piece(doc: &mut GenDoc, piece: &str) {
    doc.body.push_str(piece);
    doc.body.push('\n');
}

// ---------------------------------------------------------------------------
// Grammar
// ---------------------------------------------------------------------------

const WORDS: &[&str] = &[
    "alba", "brezza", "corallo", "duna", "estivo", "faro", "onda", "porto",
];
const INLINE_TAGS: &[&str] = &["b", "i", "em", "strong"];
const NAMED_ENTITIES: &[&str] = &["&nbsp;", "&mdash;", "&copy;", "&hellip;"];
const UNKNOWN_ENTITIES: &[&str] = &["&nope;", "&stillnope;"];

/// Inline-only element names used for structural parity assertions; none of
/// these ever anchors a block, so IR marks and output tags correspond 1:1.
const PARITY_PAIRED_KINDS: &[&str] = &["b", "i", "em", "strong", "span", "a", "sup"];
const PARITY_EMPTY_KINDS: &[&str] = &["br", "img"];

fn words(rng: &mut Rng, count: usize) -> String {
    (0..count)
        .map(|_| (*rng.pick(WORDS)).to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

/// A protected token plus its byte-exact wrapper slice.
fn guard_span(rng: &mut Rng, state: &mut GenState, doc: &mut GenDoc) -> String {
    let serial = state.next_serial();
    let token = match rng.below(6) {
        0 => format!("https://example.net/p/{serial}"),
        1 => format!("190{serial}"),
        2 => format!("lettore{serial}@example.org"),
        3 => format!("capitolo-{serial}.pdf"),
        4 => format!("#nota-{serial}"),
        _ => format!("[@fonte{serial}]"),
    };
    let slice = format!(r#"<span class="bookforge-guard">{token}</span>"#);
    doc.guard_slices.push(slice.clone());
    doc.protected_tokens.push(token.clone());
    let sentinel = format!("gx{serial}g");
    doc.visible_sentinels.push(sentinel.clone());
    format!("{slice} {sentinel}")
}

fn leaf_text(rng: &mut Rng, state: &mut GenState, doc: &mut GenDoc) -> String {
    let serial = state.next_serial();
    let sentinel = format!("sx{serial}w");
    doc.visible_sentinels.push(sentinel.clone());
    let word_count = 1 + rng.below(3);
    let mut parts = vec![words(rng, word_count), sentinel];
    match rng.below(8) {
        0 => parts.push((*rng.pick(NAMED_ENTITIES)).to_string()),
        1 => parts.push((*rng.pick(UNKNOWN_ENTITIES)).to_string()),
        2 => parts.push(format!("&#6{serial};")),
        _ => {}
    }
    parts.join(" ")
}

fn gen_inline(rng: &mut Rng, state: &mut GenState, doc: &mut GenDoc, depth: usize) -> String {
    if depth == 0 {
        return leaf_text(rng, state, doc);
    }
    match rng.below(10) {
        0..=3 => leaf_text(rng, state, doc),
        4 => guard_span(rng, state, doc),
        5..=6 => {
            let tag = rng.pick(INLINE_TAGS);
            let class = format!("calibre{}", 1 + rng.below(4));
            format!(
                "<{tag} class=\"{class}\">{}</{tag}>",
                gen_inline(rng, state, doc, depth - 1)
            )
        }
        7 => {
            let href = if rng.below(2) == 0 {
                format!("https://example.site/d/{}", state.next_serial())
            } else {
                format!("#sec{}", state.next_serial())
            };
            format!(
                "<a href=\"{href}\">{}</a>",
                gen_inline(rng, state, doc, depth - 1)
            )
        }
        8 => {
            let serial = state.next_serial();
            format!("<sup><a epub:type=\"noteref\" href=\"#n{serial}\">*{serial}</a></sup>")
        }
        _ => format!(
            "{} <span>{}</span>",
            leaf_text(rng, state, doc),
            gen_inline(rng, state, doc, depth - 1)
        ),
    }
}

fn paragraph(rng: &mut Rng, state: &mut GenState, doc: &mut GenDoc) -> String {
    let depth = 1 + rng.below(3);
    let mut inner = gen_inline(rng, state, doc, depth);
    if rng.below(3) == 0 {
        inner.push_str(" <br/>");
    }
    if rng.below(4) == 0 {
        let serial = state.next_serial();
        inner.push_str(&format!(
            r#" <img src="images/pic{serial}.png" alt="figure"/>"#
        ));
    }
    // Note: comments are never injected INSIDE a block. Replace mode owns
    // the interior of translated blocks (text and comments alike are
    // folded into the re-rendered translation), so only body-level
    // comments carry a byte-stability expectation.
    format!("<p>{inner}</p>")
}

fn heading_or_hgroup(rng: &mut Rng, state: &mut GenState, doc: &mut GenDoc) -> String {
    let level = 1 + rng.below(6);
    let text = leaf_text(rng, state, doc);
    let heading = format!("<h{level}>{text}</h{level}>");
    if rng.below(3) == 0 {
        let subtitle = words(rng, 2);
        format!("<hgroup>{heading}<p>{subtitle}</p></hgroup>")
    } else {
        heading
    }
}

fn list_block(rng: &mut Rng, state: &mut GenState, doc: &mut GenDoc) -> String {
    let items = 1 + rng.below(3);
    let mut body = String::new();
    for index in 0..items {
        // The nesting decision must be made BEFORE content generation: an
        // eager build would record sentinels it then discards.
        let nested_ol = index == 0 && rng.below(3) == 0;
        let item = if nested_ol {
            let lead_word_count = 1;
            format!(
                "<li>{}<ol><li>{}</li></ol></li>",
                words(rng, lead_word_count),
                gen_inline(rng, state, doc, 1)
            )
        } else {
            format!("<li>{}</li>", gen_inline(rng, state, doc, 2))
        };
        body.push_str(&item);
    }
    format!("<ul>{body}</ul>")
}

fn table(rng: &mut Rng, state: &mut GenState, doc: &mut GenDoc) -> String {
    match rng.below(3) {
        0 => format!(
            "<table><thead><tr><th>Year {}</th><th>Value {}</th></tr></thead><tbody><tr><td>{}</td><td>{}</td></tr></tbody></table>",
            state.next_serial(),
            state.next_serial(),
            gen_inline(rng, state, doc, 1),
            gen_inline(rng, state, doc, 1),
        ),
        // Empty-element adjacency: a self-closing sibling after a real cell.
        1 => format!(
            "<table><tr><td>{}</td><td/></tr></table>",
            gen_inline(rng, state, doc, 1)
        ),
        // Row-less stray cells anchor TableCell blocks directly.
        _ => format!("<table><td>{}</td></table>", gen_inline(rng, state, doc, 1)),
    }
}

fn aside_footnote(rng: &mut Rng, state: &mut GenState, doc: &mut GenDoc) -> String {
    let serial = state.next_serial();
    format!(
        "<aside epub:type=\"footnote\" id=\"fn{serial}\"><p>nota {}</p></aside>",
        gen_inline(rng, state, doc, 1)
    )
}

fn definition_list(rng: &mut Rng, state: &mut GenState, doc: &mut GenDoc) -> String {
    // dt/dd anchor lazily on first text, so both terms must LEAD WITH TEXT:
    // otherwise the reader would anchor the block on the innermost inline
    // element and record no marks for it (intended extraction behaviour).
    format!(
        "<dl><dt>term {} {}</dt><dd>sense {} {}</dd></dl>",
        state.next_serial(),
        gen_inline(rng, state, doc, 1),
        words(rng, 2),
        gen_inline(rng, state, doc, 1)
    )
}

fn code_block(state: &mut GenState, doc: &mut GenDoc) -> String {
    let serial = state.next_serial();
    let sentinel = format!("code{serial}x");
    doc.visible_sentinels.push(sentinel);
    format!("<pre><code class=\"lang-rust\">let code{serial}x = load();</code></pre>")
}

/// Lazy-anchor div: leading text anchors a block on the div itself, and the
/// text after the inline child exercises the stray text-node path.
fn div_lazy_text(rng: &mut Rng, state: &mut GenState, doc: &mut GenDoc) -> String {
    format!(
        "<div class=\"wrap\">{} <em>{}</em> tail</div>",
        words(rng, 2),
        leaf_text(rng, state, doc)
    )
}

fn suppressed_subtree(rng: &mut Rng, state: &mut GenState, doc: &mut GenDoc) -> String {
    let serial = state.next_serial();
    let hidden = format!("sk{serial}x");
    doc.hidden_sentinels.push(hidden.clone());
    let piece = match rng.below(4) {
        0 => {
            let slice = format!(
                "<script type=\"text/javascript\">if (tick % 3 == {serial}) {{ boot(\"{hidden}\"); }}</script>"
            );
            doc.suppressed_slices.push(slice.clone());
            slice
        }
        1 => {
            let slice =
                format!("<style type=\"text/css\">.cls{serial} {{ color: #a{serial}c; }}</style>");
            doc.suppressed_slices.push(slice.clone());
            slice
        }
        2 => {
            let slice = format!(
                "<svg xmlns=\"http://www.w3.org/2000/svg\"><rect x=\"{serial}\" y=\"2\" width=\"8\" height=\"{serial}\"/><title>{hidden}</title><path d=\"M{serial}\"/></svg>"
            );
            doc.suppressed_slices.push(slice.clone());
            slice
        }
        _ => {
            let slice = format!("<math><mi>m{serial}</mi><mo>+</mo><mn>{serial}</mn></math>");
            doc.suppressed_slices.push(slice.clone());
            slice
        }
    };
    if rng.below(3) == 0 {
        // Suppressed root nested inside an active block: the empty marker
        // pair protocol path.
        format!("<p>pre {piece} post</p>")
    } else {
        piece
    }
}

fn cdata_paragraph(state: &mut GenState, doc: &mut GenDoc) -> String {
    let serial = state.next_serial();
    let payload = format!("cdx{serial} raw & <plain> data");
    doc.cdata_payloads.push(payload.clone());
    format!("<p><![CDATA[{payload}]]> follows</p>")
}

fn entity_paragraph(rng: &mut Rng, state: &mut GenState, doc: &mut GenDoc) -> String {
    let serial = state.next_serial();
    let sentinel = format!("ex{serial}w");
    doc.visible_sentinels.push(sentinel.clone());
    let named = rng.pick(NAMED_ENTITIES);
    let unknown = rng.pick(UNKNOWN_ENTITIES);
    format!(
        "<p>uno{named}due{unknown}tre&#{};quattro&#x4{serial};{sentinel}</p>",
        65 + serial
    )
}

fn body_level_comment(rng: &mut Rng, state: &mut GenState, doc: &mut GenDoc) -> String {
    let serial = state.next_serial();
    let comment = format!("<!--ck{serial}: {}-->", rng.pick(WORDS));
    doc.comments.push(comment.clone());
    comment
}

fn empty_element(rng: &mut Rng, state: &mut GenState) -> String {
    let serial = state.next_serial();
    if rng.below(2) == 0 {
        format!("<hr class=\"rule{serial}\"/>")
    } else {
        format!("<p>br{serial} <br/> after</p>")
    }
}

fn gen_piece(rng: &mut Rng, state: &mut GenState, doc: &mut GenDoc) {
    if rng.below(6) == 0 {
        let comment = body_level_comment(rng, state, doc);
        push_piece(doc, &comment);
    }
    let piece = match rng.below(12) {
        0 | 1 => paragraph(rng, state, doc),
        2 => heading_or_hgroup(rng, state, doc),
        3 => list_block(rng, state, doc),
        4 => table(rng, state, doc),
        5 => aside_footnote(rng, state, doc),
        6 => definition_list(rng, state, doc),
        7 => code_block(state, doc),
        8 => div_lazy_text(rng, state, doc),
        9 => suppressed_subtree(rng, state, doc),
        10 => cdata_paragraph(state, doc),
        _ => {
            if rng.below(4) == 0 {
                entity_paragraph(rng, state, doc)
            } else {
                empty_element(rng, state)
            }
        }
    };
    push_piece(doc, &piece);
}

/// Fixed skeleton guaranteeing every grammar shape is present in every
/// fixture regardless of the random tail, so the invariant suite is never
/// vacuous for any seed.
fn emit_forced_skeleton(doc: &mut GenDoc, rng: &mut Rng, state: &mut GenState) {
    for shape in [
        Shape::Paragraph,
        Shape::Heading,
        Shape::List,
        Shape::TableRows,
        Shape::TableEmptySibling,
        Shape::TableStrayCell,
        Shape::Aside,
        Shape::Dl,
        Shape::Code,
        Shape::DivLazy,
        Shape::Suppressed,
        Shape::Cdata,
        Shape::Entity,
        Shape::Empty,
    ] {
        let piece = match shape {
            Shape::Paragraph => paragraph(rng, state, doc),
            Shape::Heading => heading_or_hgroup(rng, state, doc),
            Shape::List => list_block(rng, state, doc),
            Shape::TableRows => {
                let a = state.next_serial();
                let b = state.next_serial();
                format!(
                    "<table><thead><tr><th>Year {a}</th><th>Value {b}</th></tr></thead><tbody><tr><td>{}</td><td>{}</td></tr></tbody></table>",
                    gen_inline(rng, state, doc, 1),
                    gen_inline(rng, state, doc, 1)
                )
            }
            Shape::TableEmptySibling => format!(
                "<table><tr><td>{}</td><td/></tr></table>",
                gen_inline(rng, state, doc, 1)
            ),
            Shape::TableStrayCell => {
                format!("<table><td>{}</td></table>", gen_inline(rng, state, doc, 1))
            }
            Shape::Aside => aside_footnote(rng, state, doc),
            Shape::Dl => definition_list(rng, state, doc),
            Shape::Code => code_block(state, doc),
            Shape::DivLazy => div_lazy_text(rng, state, doc),
            Shape::Suppressed => suppressed_subtree(rng, state, doc),
            Shape::Cdata => cdata_paragraph(state, doc),
            Shape::Entity => entity_paragraph(rng, state, doc),
            Shape::Empty => empty_element(rng, state),
        };
        push_piece(doc, &piece);
        doc.body.push_str("\n \t \n");
    }
}

enum Shape {
    Paragraph,
    Heading,
    List,
    TableRows,
    TableEmptySibling,
    TableStrayCell,
    Aside,
    Dl,
    Code,
    DivLazy,
    Suppressed,
    Cdata,
    Entity,
    Empty,
}

fn gen_doc(rng: &mut Rng) -> GenDoc {
    let mut doc = GenDoc::default();
    let mut state = GenState::default();
    emit_forced_skeleton(&mut doc, rng, &mut state);
    for _ in 0..(4 + rng.below(5)) {
        gen_piece(rng, &mut state, &mut doc);
    }
    doc
}

// ---------------------------------------------------------------------------
// Fixture assembly
// ---------------------------------------------------------------------------

const CONTAINER_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;

const NAV_XHTML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<head><title>Navigation</title></head>
<body><nav epub:type="toc"><ol><li><a href="ch_main.xhtml">Property Fixture</a></li></ol></nav></body>
</html>"#;

fn chapter_xhtml(title: &str, body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<head><title>{title}</title></head>
<body>
{body}</body>
</html>"#
    )
}

fn content_opf() -> String {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" unique-identifier="uid" version="3.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="uid">bookforge-protocol-property</dc:identifier>
    <dc:title>Property Fixture</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="main" href="ch_main.xhtml" media-type="application/xhtml+xml"/>
    <item id="raw" href="ch_raw.xhtml" media-type="application/xhtml+xml"/>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
  </manifest>
  <spine>
    <itemref idref="main"/>
    <itemref idref="raw"/>
  </spine>
</package>"#
        .to_string()
}

fn write_epub(path: &Path, chapters: &[(&str, &str)]) {
    let file = File::create(path).expect("fixture EPUB should be creatable");
    let mut zip = ZipWriter::new(file);
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    zip.start_file("mimetype", stored)
        .expect("mimetype should start");
    zip.write_all(b"application/epub+zip")
        .expect("mimetype should write");
    zip.start_file("META-INF/container.xml", deflated)
        .expect("container should start");
    zip.write_all(CONTAINER_XML.as_bytes())
        .expect("container should write");
    zip.start_file("OEBPS/content.opf", deflated)
        .expect("opf should start");
    zip.write_all(content_opf().as_bytes())
        .expect("opf should write");
    zip.start_file("OEBPS/nav.xhtml", deflated)
        .expect("nav should start");
    zip.write_all(NAV_XHTML.as_bytes())
        .expect("nav should write");
    for (name, body) in chapters {
        let href = format!("OEBPS/{name}");
        zip.start_file(href.as_str(), deflated)
            .expect("chapter should start");
        let document = chapter_xhtml("Property Fixture", body);
        zip.write_all(document.as_bytes())
            .expect("chapter should write");
    }
    zip.finish().expect("fixture EPUB should finish");
}

fn entry_bytes(path: &Path, name: &str) -> Vec<u8> {
    let mut archive =
        ZipArchive::new(File::open(path).expect("fixture should open")).expect("zip should parse");
    let mut bytes = Vec::new();
    archive
        .by_name(name)
        .unwrap_or_else(|error| panic!("entry {name} missing: {error}"))
        .read_to_end(&mut bytes)
        .expect("entry should read");
    bytes
}

fn entry_text(path: &Path, name: &str) -> String {
    String::from_utf8(entry_bytes(path, name)).expect("entry should be UTF-8")
}

// ---------------------------------------------------------------------------
// Invariant helpers
// ---------------------------------------------------------------------------

fn marked_text(block: &Block) -> String {
    block
        .text_runs
        .iter()
        .map(|run| run.text.as_str())
        .collect::<String>()
}

/// Reader-side single-stream ordinal protocol: the ids appearing in the
/// marked text runs must equal `inline_marks` in document order, be unique,
/// and be consecutive 1..=N (paired and empty ids from ONE counter).
fn assert_single_stream_ordinals(blocks: &[Block], seed: u64) {
    for block in blocks {
        let marked = marked_text(block);
        let ids = marker_ids_in_text(&marked);
        assert_eq!(
            ids,
            block
                .inline_marks
                .iter()
                .map(|mark| mark.id.clone())
                .collect::<Vec<_>>(),
            "seed={seed} block {}: marker ids in runs diverge from inline_marks",
            block.id.0
        );

        let mut ordinals = ids
            .iter()
            .map(|id| id[1..].parse::<usize>().expect("marker ordinal suffix"))
            .collect::<Vec<_>>();
        ordinals.sort_unstable();
        let expected = (1..=ids.len()).collect::<Vec<_>>();
        assert_eq!(
            ordinals, expected,
            "seed={seed} block {}: ordinals must be unique and consecutive from one stream",
            block.id.0
        );
    }
}

/// Resolve a general entity reference exactly like the reader does:
/// numeric character references and the HTML5 named set, unknown names
/// preserved literally (`&name;`).
fn resolve_reference(reference: &quick_xml::events::BytesRef<'_>) -> String {
    if let Ok(Some(ch)) = reference.resolve_char_ref() {
        return ch.to_string();
    }
    let name = reference.decode().expect("reference should decode");
    match quick_xml::escape::resolve_html5_entity(&name) {
        Some(resolved) => resolved.to_string(),
        None => format!("&{name};"),
    }
}

/// Decoded visible text of an XHTML document: text, CDATA, and resolved
/// general references, whitespace-normalized. Protected tokens and sentinels
/// are atom-like, so containment on this string is insensitive to escaping.
fn decoded_visible_text(xhtml: &str) -> String {
    let mut reader = Reader::from_str(xhtml);
    reader.config_mut().trim_text(false);
    let mut collected = Vec::new();
    loop {
        match reader.read_event().expect("fixture XHTML should parse") {
            Event::Text(text) => {
                collected.push(
                    text.html_content()
                        .expect("text should decode")
                        .into_owned(),
                );
            }
            Event::CData(text) => {
                collected.push(text.decode().expect("cdata should decode").into_owned());
            }
            Event::GeneralRef(reference) => {
                collected.push(resolve_reference(&reference));
            }
            Event::Eof => break,
            _ => {}
        }
    }
    // Pieces are adjacent in document order: concatenating them reproduces
    // the reader-visible character stream before whitespace normalization.
    collected
        .concat()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_well_formed(xhtml: &str) -> bool {
    let mut reader = Reader::from_str(xhtml);
    reader.config_mut().trim_text(false);
    loop {
        match reader.read_event() {
            Ok(Event::Eof) => return true,
            Ok(_) => continue,
            Err(_) => return false,
        }
    }
}

fn local_tag(element: &BytesStart<'_>) -> String {
    let qualified = String::from_utf8_lossy(element.name().as_ref()).into_owned();
    qualified.rsplit(':').next().unwrap_or_default().to_string()
}

/// Structural parity: for inline-only element names, the number of output
/// start/empty tags in the translated document must equal the number of
/// reader-side marks of that kind. Attribute bytes ride inside template
/// events, so equal counts with skip-free rendering pins structural fidelity.
fn assert_element_parity(out_main: &str, main_blocks: &[Block], seed: u64) {
    let mut expected: HashMap<String, usize> = HashMap::new();
    for block in main_blocks {
        for mark in &block.inline_marks {
            let paired = mark.id.starts_with('m');
            let tracked = paired && PARITY_PAIRED_KINDS.contains(&mark.kind.as_str())
                || !paired && PARITY_EMPTY_KINDS.contains(&mark.kind.as_str());
            if tracked {
                *expected.entry(mark.kind.clone()).or_default() += 1;
            }
        }
    }

    let mut actual: HashMap<String, usize> = HashMap::new();
    let mut reader = Reader::from_str(out_main);
    reader.config_mut().trim_text(false);
    loop {
        match reader.read_event().expect("output should parse") {
            Event::Start(element) => {
                *actual.entry(local_tag(&element)).or_default() += 1;
            }
            Event::Empty(element) => {
                *actual.entry(local_tag(&element)).or_default() += 1;
            }
            Event::Eof => break,
            _ => {}
        }
    }

    for kind in PARITY_PAIRED_KINDS.iter().chain(PARITY_EMPTY_KINDS) {
        assert_eq!(
            actual.get(*kind).copied().unwrap_or(0),
            expected.get(*kind).copied().unwrap_or(0),
            "seed={seed}: '{kind}' tag count in output diverges from reader marks"
        );
    }
}

// ---------------------------------------------------------------------------
// Per-seed driver
// ---------------------------------------------------------------------------

const RAW_ENTRY_NAMES: &[&str] = &[
    "mimetype",
    "META-INF/container.xml",
    "OEBPS/content.opf",
    "OEBPS/nav.xhtml",
    "OEBPS/ch_raw.xhtml",
];

fn run_seed(seed: u64, root: &Path) {
    let mut rng = Rng::new(seed.wrapping_mul(SEED_MULTIPLIER).wrapping_add(SEED_OFFSET));
    let main = gen_doc(&mut rng);
    let raw = gen_doc(&mut rng);

    let dir = root.join(format!("seed-{seed}"));
    fs::create_dir_all(&dir).expect("seed dir should create");
    let input = dir.join("in.epub");
    write_epub(
        &input,
        &[("ch_main.xhtml", &main.body), ("ch_raw.xhtml", &raw.body)],
    );

    // ---- reader + IR invariants -------------------------------------------
    let book =
        read_epub(&input).unwrap_or_else(|error| panic!("seed={seed}: read failed: {error}"));
    assert!(
        !book.blocks.is_empty(),
        "seed={seed}: generated corpus must produce blocks"
    );
    build_segments(&book, &SegmentationConfig::default())
        .unwrap_or_else(|error| panic!("seed={seed}: segmentation failed: {error}"));
    assert_single_stream_ordinals(&book.blocks, seed);

    // Suppressed-subtree interiors must stay out of the translatable
    // payload entirely (reader-side guarantee). The rebuilt document still
    // carries those bytes verbatim by design; byte stability of the raw
    // slices is asserted separately below.
    let translatable_payload = book
        .blocks
        .iter()
        .map(marked_text)
        .collect::<Vec<_>>()
        .join(" ");
    for hidden in main.hidden_sentinels.iter().chain(&raw.hidden_sentinels) {
        assert!(
            !translatable_payload.contains(hidden.as_str()),
            "seed={seed}: suppressed-subtree text {hidden} leaked into translatable text"
        );
    }

    let inspection =
        inspect_epub(&input).unwrap_or_else(|error| panic!("seed={seed}: inspect failed: {error}"));
    assert!(
        inspection.has_nav,
        "seed={seed}: nav fixture must be detected"
    );

    let main_section_ids = book
        .sections
        .iter()
        .filter(|section| section.href.ends_with("ch_main.xhtml"))
        .map(|section| section.id.clone())
        .collect::<HashSet<_>>();
    let main_blocks = book
        .blocks
        .iter()
        .filter(|block| main_section_ids.contains(&block.section_id))
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        !main_blocks.is_empty(),
        "seed={seed}: main chapter must yield blocks"
    );

    let translations = main_blocks
        .iter()
        .map(|block| BlockTranslation {
            block_id: block.id.clone(),
            text: format!("{APPLIED_MARK}{}", marked_text(block)),
        })
        .collect::<Vec<_>>();

    // ---- rebuild #1: replace mode, no language rewrite ---------------------
    let out_default = dir.join("out-default.epub");
    rebuild_epub_with_options(
        &book,
        &translations,
        &out_default,
        &RebuildOptions {
            mode: BilingualMode::Replace,
            ..RebuildOptions::default()
        },
    )
    .unwrap_or_else(|error| panic!("seed={seed}: default rebuild failed: {error}"));

    // RAW untouched-entry bytes are byte-stable: untouched spine documents
    // and metadata are copied through unchanged.
    for name in RAW_ENTRY_NAMES {
        assert_eq!(
            entry_bytes(&out_default, name),
            entry_bytes(&input, name),
            "seed={seed}: raw entry {name} diverged"
        );
    }

    let out_main = entry_text(&out_default, "OEBPS/ch_main.xhtml");
    assert!(
        is_well_formed(&out_main),
        "seed={seed}: default rebuild produced malformed XHTML"
    );
    assert_eq!(
        out_main.matches(APPLIED_MARK).count(),
        main_blocks.len(),
        "seed={seed}: a block was skipped instead of rendered (missing translation prefix)"
    );
    assert_element_parity(&out_main, &main_blocks, seed);

    let decoded = decoded_visible_text(&out_main);
    for sentinel in &main.visible_sentinels {
        assert!(
            decoded.contains(sentinel.as_str()),
            "seed={seed}: visible sentinel {sentinel} lost in rebuild"
        );
    }
    for token in &main.protected_tokens {
        assert!(
            decoded.contains(token.as_str()),
            "seed={seed}: protected span {token} missing after replace"
        );
    }
    for payload in &main.cdata_payloads {
        assert!(
            decoded.contains(payload.as_str()),
            "seed={seed}: CDATA payload lost after replace"
        );
    }
    for comment in &main.comments {
        assert!(
            out_main.contains(comment.as_str()),
            "seed={seed}: comment bytes diverged: {comment}"
        );
    }
    for slice in &main.suppressed_slices {
        assert!(
            out_main.contains(slice.as_str()),
            "seed={seed}: suppressed subtree not byte-stable: {slice}"
        );
    }
    for slice in &main.guard_slices {
        assert!(
            out_main.contains(slice.as_str()),
            "seed={seed}: protected-span wrapper not byte-stable: {slice}"
        );
    }

    // ---- rebuild #2: replace mode with a target language -------------------
    let out_lang = dir.join("out-lang.epub");
    rebuild_epub_with_options(
        &book,
        &translations,
        &out_lang,
        &RebuildOptions {
            target_language: Some("Italian".to_string()),
            mode: BilingualMode::Replace,
            ..RebuildOptions::default()
        },
    )
    .unwrap_or_else(|error| panic!("seed={seed}: language rebuild failed: {error}"));

    let lang_main = entry_text(&out_lang, "OEBPS/ch_main.xhtml");
    assert!(
        is_well_formed(&lang_main),
        "seed={seed}: language rebuild produced malformed XHTML"
    );
    assert!(
        lang_main.contains(r#"lang="it""#),
        "seed={seed}: target language must be applied to the document root"
    );
    assert_eq!(
        lang_main.matches(APPLIED_MARK).count(),
        main_blocks.len(),
        "seed={seed}: language rebuild skipped a block"
    );
    for slice in main.suppressed_slices.iter().chain(&main.guard_slices) {
        assert!(
            lang_main.contains(slice.as_str()),
            "seed={seed}: raw slice not byte-stable under language rebuild: {slice}"
        );
    }
    let lang_decoded = decoded_visible_text(&lang_main);
    for token in &main.protected_tokens {
        assert!(
            lang_decoded.contains(token.as_str()),
            "seed={seed}: protected span {token} lost under language rebuild"
        );
    }

    // ---- determinism: two identical rebuilds are byte-identical ------------
    let out_again = dir.join("out-again.epub");
    rebuild_epub_with_options(
        &book,
        &translations,
        &out_again,
        &RebuildOptions {
            mode: BilingualMode::Replace,
            ..RebuildOptions::default()
        },
    )
    .unwrap_or_else(|error| panic!("seed={seed}: repeat rebuild failed: {error}"));
    assert_eq!(
        fs::read(&out_default).expect("first rebuild should read"),
        fs::read(&out_again).expect("repeat rebuild should read"),
        "seed={seed}: rebuild output is not deterministic"
    );

    let _ = fs::remove_dir_all(&dir);
}

fn run_batch(first_seed_index: u64, count: u64, label: &str) {
    let started = Instant::now();
    let root = std::env::temp_dir().join(format!("bf-protocol-prop-{label}"));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("batch root should create");
    for index in first_seed_index..(first_seed_index + count) {
        run_seed(index, &root);
    }
    let elapsed = started.elapsed();
    println!("batch {label}: {count} seeds in {elapsed:?}");
    assert!(
        elapsed < Duration::from_secs(15),
        "batch {label} exceeded the CI-time budget: {elapsed:?}"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn property_batch_seeds_000_049() {
    run_batch(0, 50, "000-049");
}

#[test]
fn property_batch_seeds_050_099() {
    run_batch(50, 50, "050-099");
}

#[test]
fn property_batch_seeds_100_149() {
    run_batch(100, 50, "100-149");
}

#[test]
fn property_batch_seeds_150_199() {
    run_batch(150, 50, "150-199");
}
