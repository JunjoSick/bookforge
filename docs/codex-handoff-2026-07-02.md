# Handoff to Codex — 2026-07-02

Written by Claude Code on behalf of the maintainer. Four tasks. Order:
task 1 (commit/release) first, then task 1b (translated-book whitespace
bug — **the maintainer's top bug**), then 2 and 3. Task 1 must land before
task 3 starts (task 3 creates a lot of new diff and the in-flight work
should not be tangled into it). Task 1b is a real output-quality bug and
may justify folding into the v2.0.3 release or a fast-follow v2.0.4.

Ground rules (from `docs/ROADMAP.md` §1, non-negotiable):

- The LLM never sees or produces raw XHTML/PDF structure — only validated
  JSON prose payloads. Reassembly is deterministic pure code.
- No dependencies that break single-static-binary distribution.
- CLI flags and JSONL event fields follow semver within a major line.

---

## Task 1 — Commit the in-flight work, cut v2.0.3

The working tree currently has two independent, finished changes. Commit
them as **separate commits**:

1. **Broken-pipe / CLI-parse hardening** — `crates/bookforge-cli/src/main.rs`
   and `crates/bookforge-cli/src/progress.rs`. Adds `parse_cli()` with
   broken-pipe-tolerant error printing, `write_default_help()`, and
   `is_broken_pipe()`, plus tests. Also the `docs/ROADMAP.md` update
   (status header + v2.0 row) can ride with this or go in its own
   `docs:` commit.
2. **Dashboard library-layout fix** — `crates/bookforge-cli/src/commands/serve.rs`,
   three CSS changes already made:
   - `.book-grid` → `repeat(auto-fill,minmax(320px,1fr))` (was rigid `1fr 1fr`)
   - `.pagehead` → added `flex-wrap:wrap`
   - `.add-card` → `min-height:112px` (was 140px, mismatched book-card height)

Verify before committing: `cargo check -p bookforge-cli --all-features`
(already passing) and `cargo test -p bookforge-cli`.

Then cut a patch release. Release process for this repo:

- `cargo set-version --workspace 2.0.3` (workspace version bump)
- Update `CHANGELOG.md` following its existing format
- Commit as `chore(release): v2.0.3` (see `git log` for precedent)
- Tag `v2.0.3` and push the tag — **the tag push triggers cargo-dist CI**,
  which builds and publishes the release artifacts. No manual release steps.

Fold task 2 into this release if you do task 2 first; otherwise v2.0.3 with
tasks 1's content is fine and task 2 can wait for the next patch.

## Task 1b — Fix whitespace loss at inline-marker boundaries (translated EPUBs)

**Symptom (maintainer report: "translated book layout seems odd sometimes").**
In translated output, words glue together across inline formatting
boundaries — e.g. the source `…let's head to</span> <span><i>Thanatos</i>`
becomes `…verso<span><i>Thanatos</i>` (no space, renders as
"versoThanatos"). The inverse also appears: a trailing space kept inside
`<i>…Culture </i>` followed by LLM-placed punctuation yields "Culture ,".

**Measured repro.** Source `~/Downloads/CCRU_Abstract_Culture_2024.epub`
vs its translation
`~/Downloads/Telegram Desktop/ccru-abstract-culture-2024.it.deepseek-v4-flash.epub`.
Unzip both; count glued inline boundaries with:

```
grep -oE "[a-zàèéìòù]</span><span[^>]*>(<i[^>]*>)?[A-Za-zÀ-ù]" <dir>/index_split_*.html | wc -l
```

Source: 1 hit. Translation: **47 hits**. Paragraph counts per file match
exactly (structure preservation is working; only inter-marker whitespace
is lost).

**Root cause.** `crates/bookforge-epub/src/reader.rs`,
`BlockBuilder::push_text` (~line 753): a whitespace-only text node
between two inline elements is not emitted as its own run *between* the
marker tokens — instead the fallback appends a space to the last
non-marker run, i.e. **inside** the preceding marker span. The marked
text sent to the model becomes `…verso </m5><m6>…`; models routinely trim
trailing whitespace before a closing marker, and the writer
(`render_marked_translation`, `crates/bookforge-epub/src/writer.rs:568`)
reassembles exactly what the model returned, so the word boundary is gone.

**Fix direction (both halves, belt and suspenders):**

1. Reader: emit the separating space as its own text run positioned
   *between* the two marker tokens (`</m5> <m6>`) instead of appending it
   inside the previous span.
2. Writer: deterministic guard after reassembly — for each adjacent
   inline-element boundary that had intervening whitespace in the original
   events, if the rendered translation butts a word character directly
   against that boundary, reinsert a single space. This protects against
   the model eating the space regardless of where the reader puts it.

Cache note: if the reader change alters the marked text sent to the model,
it changes prompt payloads — check whether `CACHE_KEY_SCHEMA_VERSION` /
prompt-contract versioning (ROADMAP §1.3) needs a bump per the project's
own rules.

**Acceptance:** unit tests in bookforge-epub covering `a</span> <span>b`,
`a</i> <i>b`, and `&nbsp;`-only nodes round-tripping with a mock
translation that strips marker-adjacent whitespace; re-translating the
CCRU book (or a fixture distilled from it) produces a glued-boundary
count comparable to the source. Existing tests stay green.

**Related but separate (do NOT fix here):** the CCRU source is a
PDF-conversion EPUB where every printed *line* is its own `<p>`, so the
translation inherits English line-lengths and reads ragged in Italian
(mid-clause paragraph breaks). That is structure-faithful behavior, not a
bug; the fix is an opt-in reflow/paragraph-merge pass and belongs in the
v1.6 design discussion (task 3) — raise it there with the maintainer
before implementing anything.

## Task 2 — Responsive sweep over the remaining dashboard grids

The dashboard is a single embedded HTML/CSS/JS string in
`crates/bookforge-cli/src/commands/serve.rs` (CSS starts ~line 1229).
It has **zero `@media` queries**. The library grid is fixed (task 1),
but these are still rigid and cramp on narrow windows:

- `serve.rs:1388` `.stat-grid` — `repeat(4,1fr)` (progress screen stats)
- `serve.rs:1345` `.facts` — `1fr 1fr` (wizard review step)
- `serve.rs:1356` `.adv-grid` — `1fr 1fr` (wizard advanced options)
- `serve.rs:1363` `.modelcards` — `1fr 1fr` (wizard model picker)
- `serve.rs:1302-1303` `.wiz` / `.rail` — fixed 236px side rail; below
  ~700px the wizard panel gets too narrow. Stack the rail on top
  (or collapse it) under a breakpoint.

Approach: prefer `repeat(auto-fill,minmax(<sane-min>,1fr))` where the
children are uniform cards (stat tiles ~150px min, model cards ~220px min);
use one `@media (max-width:~720px)` block for the wizard rail and anything
auto-fill can't express. Match the existing CSS style exactly: single-line
rules, no spaces after `:` or `,`, CSS variables from the `:root` block.

Verify visually: `cargo run -p bookforge-cli --features serve -- serve`,
open the printed URL, and check library/wizard/progress screens at full
width and at a ~500px-wide window. The wizard and progress screens are
reachable without running a real job (progress needs an existing job in
`.bookforge/`; if none, at minimum check the wizard).

## Task 3 — Start v1.6: PDF ingestion hardening

This is the next roadmap milestone and the maintainer's top priority
(scientific papers with figures/tables, and unorthodox-layout scanned
books). **The spec is `docs/ROADMAP.md` §9 (lines ~1827-1985) — read it
in full before writing code; it defines goal, phases, deliverables, CLI
surface, out-of-scope, and acceptance criteria.** Existing code lives in
`crates/bookforge-pdf`.

Instructions from the maintainer's process doc: follow the phases in §9.4
in order, do not skip ahead, and if a needed detail is missing from the
roadmap, stop and ask the maintainer rather than inventing it.

Work on a branch (e.g. `v1.6-pdf-hardening`) and open a PR; do not push
this to `main` directly. Respect the §9.6 out-of-scope list strictly.

## Task 4 (added 2026-07-03) — PR #22 fix pass, from Claude's code review

A multi-angle review of `main...v1.6-pdf-hardening` found 7 confirmed and
3 plausible correctness bugs plus confirmed cleanup items. Fix on the same
branch; PR #22 stays draft until the acceptance step at the end passes.

**Common theme — read this first.** Every confirmed bug lives beyond the
stubbed-poppler test boundary: the tests stub pdftoppm/pdfimages and assert
the *arguments* passed to them, so real-tool geometry, file counts, and
enumeration order are never exercised. Fixes must come with the acceptance
step below, not just more stub tests.

### Confirmed bugs (fix all, in this order)

1. **Crop coordinate mismatch (`tools.rs:234`, `render_page_crop_png`).**
   Crop rects are in pdftohtml XML units (default zoom 1.5 ≈ 108 dpi; a
   US-Letter page is 918 units wide, see `parse.rs` fixture), but they are
   passed verbatim as pdftoppm `-x/-y/-W/-H` pixel flags at `-r 150`
   (1275 px wide) — every crop is misplaced/mis-scaled by 150/108 ≈ 1.389×.
   Fix: scale coords by (render_dpi / 108) before passing, or pass
   `-zoom 1.5`-consistent dpi to pdftoppm (`-r 108`); either way derive both
   numbers from ONE shared constant, and pin pdftohtml's zoom explicitly
   (`-zoom 1.5`) so the assumption is stated in the invocation.

2. **`remove_blocks_in_region` deletes unrelated text (`convert.rs:1232`).**
   Removal keys only on `anchor.top` within the region's *padded* vertical
   band; `BlockAnchor` has no horizontal fields, so on two-column pages it
   deletes the other column's paragraphs, and the 8–48px padding swallows
   the following paragraph. The crop PNG is horizontally bounded, so the
   deleted text is NOT preserved anywhere. Fix: add left/width to
   `BlockAnchor` (populate in `reconstruct.rs` where anchors are built) and
   require horizontal overlap with the *unpadded* region; use the padded
   rect only for the raster crop, not for deletion.

3. **pdfimages page misalignment (`tools.rs:159`).** `parse_pdfimages_list`
   keeps only `type=="image"` rows, but `pdfimages -png` also writes files
   for smask/stencil/mask objects; paths are then zipped positionally with
   no length check, so one mask shifts every later image's page/dims (and
   overflow gets page 0). Fix: don't filter the -list rows — keep all rows
   aligned with the emitted files, pair by the `num` column / filename index
   (`-NNN.png` suffix), then discard non-"image" entries AFTER pairing.
   Add a hard error (or at minimum a report warning) if counts still diverge.

4. **Positional image↔region pairing (`convert.rs:919`).** The Nth
   extracted image on a page is paired with `page.images[N]`; pdfimages
   enumerates in object order, pdftohtml in draw order. Fix: match by best
   dimension/aspect-ratio fit (extracted image width/height vs region
   width/height) with a sanity threshold, falling back to unmatched
   (region=None) rather than a wrong pairing.

5. **Region-less images become spurious figures (`convert.rs:857`).**
   Candidates with no pdftohtml region are emitted as caption-less figures
   anchored at `top=i32::MAX` with no size/decorative filter — masks,
   logos, and background rasters get appended to every page. Fix: drop
   region-less images below a minimum pixel area, drop images whose bytes
   repeat across many pages (running ornament/logo), and record dropped
   ones in the conversion report instead of the EPUB.

6. **Image tooling is now a fatal dependency (`tools.rs:67`,
   `convert.rs:71`).** `discover()` hard-requires pdfimages+pdftoppm, and
   `extract_images()?` runs before the text baseline — a minimal poppler
   install or one malformed embedded image aborts conversions that
   succeeded on v2.0.3 text-only. Fix: make the two image tools optional
   in `PopplerTools` (Option<PathBuf>); when missing or when extraction
   fails, log/report a degraded-mode warning and continue text-only.
   `doctor` should list them as "recommended (figure preservation)", not
   required.

7. **Coverage under-report (`convert.rs:103`).** `reconstructed_chars` is
   summed after `remove_blocks_in_region` deleted table/equation text that
   IS preserved (as crops), so `coverage_percent` drops and the sub-95%
   warning fires about content that survived. Fix: count removed-to-media
   chars separately and either credit them to coverage or report them as
   an explicit "preserved as images: N chars" line; do not let media
   preservation read as text loss. (Note: per-page `stats.chars` is
   pre-removal, so low-confidence detection is unaffected — keep it so.)

### Plausible bugs (fix or consciously accept with a comment + report warning)

8. **Equation detector rasterizes math-dense prose (`convert.rs:705`).**
   "(p = 0.05)" centered on its own line passes every gate (3 symbols, 8
   nonspace, 9≥8). Tighten: require at least one *strong* operator beyond
   parens/hyphens for the density rule, or exempt fragments that parse as
   a single parenthetical.
9. **Table detector on aligned non-table content (`convert.rs:567`).**
   Aligned author/affiliation blocks or stats lists (3+ fragments/row, 3+
   rows, digits) rasterize and delete real prose. Consider requiring a
   nearby table caption OR ≥4 rows, and always report which text was
   removed so the user can audit.
10. **Caption ranking saturation (`convert.rs:1109`).**
    `top.saturating_sub(bottom)` gives distance 0 to fragments in the
    [bottom-8, bottom) window, beating the true caption below. Rank by
    absolute distance from the image bottom instead.

### Confirmed cleanup (same pass, keeps the above fixed-for-good)

- Replace the hand-synchronized parallel `Vec<DocBlock>` +
  `Vec<BlockAnchor>` with one `Vec<AnchoredBlock>` (mutation sites:
  `convert.rs:1217/1234/1258/214`, `reconstruct.rs:504`) — this is the
  root enabler of anchor-desync bugs and makes fix #2 safer.
- One low-confidence threshold: `convert.rs:23` (0.95) and `report.rs:58`
  (95.0) must derive from a single shared constant.
- Batch pdftoppm: render each page with regions once and crop in memory
  (or one range render for preserved pages) instead of one subprocess per
  region/page (`convert.rs:401/1022/181`).
- Deduplicate `scoped_temp_dir` (convert.rs:1276 vs tools.rs:338 — make
  the tools.rs one pub(crate)), `normalize_caption` (vs
  `reconstruct::normalize_running_text`), and `fragment_text` (vs
  `Line::text()`/`DocBlock::text()`).
- Unify math-symbol classification: `convert.rs:729 is_math_symbol` vs
  `bookforge-epub reader.rs:1361 is_inline_math_operator` diverge (minus,
  brackets, ∑∫√∂∇∞∈ missing on the epub side), so an expression can be
  rasterized by one stage and left unprotected by the other. Share one
  definition (bookforge-core is the natural home).

### Acceptance for task 4 (blocking)

1. All existing tests green; new regression tests for fixes 1–5 and 7
   that assert real geometry/pairing (not just stub argv).
2. **Real-poppler end-to-end run:** convert an actual scientific PDF
   (the BERT paper, arXiv 1810.04805 — the fixtures are modeled on it)
   with real pdftohtml/pdfimages/pdftoppm installed; open the EPUB and
   confirm figure/table/equation crops show the right content at the
   right size, no spurious logo/mask figures, no missing paragraphs
   around tables in the two-column layout, and a sane coverage number in
   the report. Record the result in the PR description.
3. Text-only degraded mode: rename pdfimages away, convert a text PDF,
   confirm an EPUB is still produced with a warning.

## Task 5 (added 2026-07-03) — heuristic fixes from the real BERT acceptance run

Task 4 shipped and was verified end-to-end against arXiv 1810.04805 with
real poppler (commit 682aa8f): crop geometry, coverage crediting, and the
degraded text-only mode are all confirmed good. The visual read-through
found three remaining heuristic problems. The test PDF is available at
`/tmp/claude-1000/-home-junjo-Desktop-tRustTheProcess/0e5a3f15-2fec-4135-967b-aea61ff5e4d7/scratchpad/bert.pdf`
(775166 bytes, 16 pages, A4) — use it for the acceptance re-run; if
unreadable from the sandbox, distill XML fixtures from it as before.

### 5.1 Vector-figure regions are too greedy (page 16 evidence)

`pdf-figure-0005.png` captured the intended chart (bottom-left column,
"MNLI Dev Accuracy" plot) PLUS the section heading "C.2 Ablation for
Different Masking Procedures" and full paragraphs from BOTH text columns.
Those paragraphs (one of 119 chars) were then removed as "preserved as
image" — translatable prose became untranslatable pixels. The audit
warnings from Task 4 itemize exactly which text was absorbed (grep the
convert output for "preserved as image").

Fix direction, in `vector_figure_regions` / region growth:
- Build the region from the chart-label fragments and drawn-graphic
  extents only; do not let caption width or the 360px lookback drag in
  full-width bands.
- On pages detected as two-column (`two_column` per-page stat already
  exists), clamp the region horizontally to the column that contains the
  labels' centroid.
- Never absorb prose-like fragments into a region: a fragment with (say)
  > 6 words and sentence punctuation is prose; if it would fall inside a
  candidate region, shrink the region below it rather than swallow it.
  If shrinking is impossible, keep the block as text and warn — for a
  translation tool, wrongly-imaged prose is worse than a slightly
  cropped chart.

### 5.2 Table/equation detectors fire inside figures

Evidence: `pdf-table-0007.png` and `pdf-table-0008.png` are horizontal
strips sliced out of Figure 1's diagram (aligned E_[CLS]/E_1/…/E_N token
boxes read as "aligned numeric cells"); the single equation crop
`pdf-equation-0001.png` is actually the table header cell "MNLI-(m/mm)
392k" — no display equation at all.

Fix direction:
- Exclude any row/fragment that intersects an extracted-image region or
  a detected figure region from table and equation candidacy — media
  regions must be disjoint by construction (one classification per area,
  figures win over tables over equations).
- Investigate which gate admitted "MNLI-(m/mm)": determine what counted
  as the strong operator ('/'? '-'?) and tighten `has_strong_operator`
  to genuine relational/aggregation operators (=, <, >, ≤, ≥, ∑, ∫, …) —
  not slashes, hyphens, or parens. Add "MNLI-(m/mm) 392k" as a negative
  fixture.
- Diagram token rows: add the Figure 1 strip as a negative fixture for
  the table detector (distill from the BERT XML).

### 5.3 Cluster diagram sub-images into one figure

The paper's ~5 real figures became 52 figure blocks because each vector
diagram is drawn with many small raster XObjects (38 extracted images:
arrows, ellipsis dots, colored boxes — see `pdf-image-0035.png`, a strip
of ellipses). Each currently becomes its own Figure block in the reading
flow.

Fix direction:
- Cluster image regions on the same page whose rects overlap or sit
  within ~24 XML units of each other into one composite region; render
  ONE crop for the cluster (the padded union rect) and emit ONE figure
  block.
- If a cluster (or single image) falls inside an already-detected
  vector-figure region, it is part of that figure — drop it, don't emit
  it separately.
- Keep the Task 4 decorative/repeated-raster filtering for what remains.

### Acceptance for task 5 (blocking)

1. Unit fixtures: page-16 two-column chart (region must exclude both
   prose columns), Figure 1 token-row strip (not a table), "MNLI-(m/mm)
   392k" (not an equation), and a multi-raster diagram page (one figure
   block out).
2. Re-run the BERT conversion (command as in Task 4 acceptance): report
   must show roughly 5-8 figure blocks (not 52), tables only for the real
   tables, no equation crops unless a real display equation is found, and
   no "preserved as image" warning containing a prose paragraph (> ~60
   chars with sentence punctuation) — chart labels and captions are fine.
3. Full workspace check/test/clippy/fmt clean. Commit in logical units.
   Claude does the final visual pass on the crops before the PR leaves
   draft.

## Task 6 (added 2026-07-03) — cosmetic crop polish (final v1.6 pass)

Task 5 verified good end-to-end (BERT: figures 52→11, no prose imaged;
Fisher's Acid Communism 100% clean; Flatline Constructs 99.3% with correct
spread reading order). Three cosmetic items remain from the visual pass —
none lose text, all are crop-quality. Fix on `v1.6-pdf-hardening`.

### 6.1 Clamp table regions to their column

Table crops on two-column pages span the full page width, pulling the
neighboring column's content into the image (BERT `pdf-table-0002.png`
mixes two different tables; `pdf-table-0005.png` shows body prose from
the other column). Apply the same column-clamping Task 5.1 added for
vector figures: clamp the table region horizontally to the column
containing the tableish rows. The neighboring text is NOT removed from
the flow (coverage unaffected) — this is purely about the crop rect.

### 6.2 Shrink vector-figure crops above prose

The page-16 chart crop (`pdf-figure-0009.png`) still *shows* the section
heading and intro paragraph above the chart (the text correctly stays in
the flow, so it appears twice: once as pixels, once as text). Task 5.1
stopped absorbing prose into the region for removal purposes; also shrink
the crop rect itself: the region's top should start at the topmost
chart-label/graphic fragment, not at prose above it.

### 6.3 Include outermost sub-images in cluster unions

Two BERT diagrams are clipped at the edges: `pdf-figure-0006.png` loses
Figure 2's leftmost token column, `pdf-figure-0007.png` loses Figure 3's
third panel (ELMo). Likely the outermost raster sub-images fall outside
the cluster distance threshold or land in a second cluster that then gets
dropped/merged wrong. Diagnose against the BERT XML (pages 3 and 13-ish);
fix so a diagram's full horizontal extent is covered — consider unioning
clusters whose rects vertically overlap on the same page.

### Acceptance for task 6 (blocking)

1. BERT re-run: table crops contain a single column's content; the
   page-16 figure crop starts at the chart (no heading/prose visible);
   Figures 2 and 3 crops show the complete diagrams (all panels/columns).
   Counts stay in the Task 5 ranges (figures ~5-11, tables ~6, 0
   equations) and coverage stays 100.0%.
2. Fisher regression: converting `test/Acid_Communism.pdf` and
   `test/Flatline_Constructs.pdf` yields coverage >= the current values
   (100.0% / 99.3%) with no new warnings.
3. Full workspace check/test/clippy/fmt clean; fixtures for 6.1-6.3 where
   distillable. Commit in logical units; do not push; PR #22 stays as-is
   (already ready) — Claude re-verifies visually before pushing.

## Verification expectations (all tasks)

- `cargo check --workspace --all-features`, `cargo test --workspace`,
  `cargo clippy --workspace --all-features` clean before each commit.
- Conventional-commit style messages matching `git log` precedent.
