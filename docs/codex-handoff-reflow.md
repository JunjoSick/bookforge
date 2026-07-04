# Handoff to Codex — §9c source-EPUB reflow (2026-07-04)

Written by Claude Code on behalf of the maintainer. First of two
quality-of-life milestones scheduled after v2.2.0 (the other is §10.1.1
pause/stop, which will follow on its own branch).

**The authoritative spec is `docs/ROADMAP.md` §9c (freshly promoted from
sketch to spec — read it in full before writing code).** It defines the
command surface, the six-condition merge rule, dehyphenation, the report
format, and acceptance criteria. If a needed detail is missing, stop and
ask the maintainer rather than inventing it.

## Context — why this exists

Third-party PDF→EPUB conversions (Calibre et al.) emit one `<p>` per
printed line. BookForge correctly preserves structure, so translations
inherit English-line-length paragraph breaks and read ragged. This is a
*source-quality repair* tool for EPUBs BookForge did not create. The
motivating book is `test/CCRU_Abstract_Culture_2024.epub` (restored to
the repo working tree; do not commit EPUBs).

## Non-negotiables (ROADMAP §1 + §9c)

- **Reflow is a standalone preprocessing command.** It must NOT touch
  the translate pipeline, prompts, cache keys, or reassembly. If you
  find yourself editing translate/resume/reader marker logic, you have
  gone off-spec.
- Opt-in forever: no default-on behavior anywhere.
- Output EPUB must be EPUBCheck-valid and preserve everything the merge
  rule doesn't explicitly change (metadata, spine, images, CSS, non-`<p>`
  content byte-preserved as far as the existing writer infra allows).
- Every merge is auditable via the JSON report (§9c.3).
- Single-static-binary rule: no new C deps; use the existing
  quick-xml/zip machinery in `bookforge-epub`.

## Where the work lives

- `crates/bookforge-epub` — new `reflow.rs` module (the merge engine +
  report types). Reuse the crate's existing EPUB open/rewrite plumbing
  (see `writer.rs` for how resources are parsed and re-serialized with
  quick-xml, and `validate.rs` for the validation harness).
- `crates/bookforge-cli/src/commands/` — new `reflow.rs` command wired
  into `main.rs`/`mod.rs` alongside `convert`/`validate`. Flags per
  §9c.1: `--output`, `--report`, `--dry-run`.
- No serve.rs / dashboard / TUI work in this milestone.

## Merge rule notes (read §9c.2 for the binding text)

- Only consecutive sibling `<p>` pairs, chained A+B+C… while each pair
  qualifies.
- Terminal-punctuation test must handle closing quotes/brackets after
  the terminal mark (`."` `.”` `?»` etc. are terminal).
- Lowercase test is Unicode-aware (`char::is_lowercase`), not ASCII.
- Dehyphenation: trailing `-` attached to a word char → join with no
  space and drop the hyphen.
- Guards: equal `class` (or both absent), no `id` on B, no nested block
  elements / images / empty text in either, nothing but whitespace
  between them.
- B's inline children append to A's children verbatim; A's attributes
  win.

## Working agreement

- Branch: `feat/reflow` (already created and checked out; roadmap spec
  + this handoff are committed on it).
- Conventional commits, logical units, tests with each commit.
- Full workspace `cargo check` / `test` / `clippy` / `fmt` clean before
  each commit.
- Unit tests: each of the six merge conditions individually blocking a
  merge; chaining; dehyphenation; terminal-punct-with-closing-quote;
  Unicode lowercase; report record contents; dry-run writes no EPUB.
- End-to-end: run `bookforge reflow` on
  `test/CCRU_Abstract_Culture_2024.epub`, check the paragraph count
  drops substantially and EPUBCheck passes (if on PATH; also run the
  BookForge validator). Then run it on `test/AcidCommunism.v16.epub`
  and confirm zero or near-zero merges (guard sanity).
- Do not push; leave the branch ready for Claude's review pass.

## Acceptance (from §9c.4, condensed)

CCRU output reads as prose with far fewer `<p>`s and passes
EPUBCheck/validator; healthy EPUBs are untouched (≈0 merges); dry-run
report matches a real run; the final un-mockable read-a-chapter check
is the maintainer's/Claude's job, not yours.
