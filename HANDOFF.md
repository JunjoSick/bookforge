# BookForge session handoff — release update 2026-06-20

This file was recreated after another chat appears to have cleaned the
workspace. Treat **current filesystem state** as authoritative; the
historical PDF media notes below are preserved so the work can be
reconstructed, not because the source still exists.

Do not commit or push without the owner asking. Never `git add -A`.
`tests/` at the repo root is gitignored and may contain private API keys
and books; never print its contents.

## 0. v1.7.0 release update

- Release preparation moved to `codex/release-v1.7.0` from current
  `origin/main`.
- All six workspace packages and internal dependency requirements are
  aligned to `1.7.0`.
- This is a version-alignment release. Bilingual output remains planned;
  it is not implemented or claimed by v1.7.0.
- The latest roadmap and repository ignore rules are included.

## 1. Current repository state

- Main worktree: `v1.5-extraction-and-scheduling` at `69ed0c1`.
  This checkout is stale relative to the released `main` branch.
- Authoritative release state: `origin/main` at `1ce614b`, tagged
  `v1.5.0`.
- GitHub release `v1.5.0` exists and cargo-dist produced release assets.
- crates.io `v1.5.0` is published for all workspace crates:
  `bookforge-core`, `bookforge-epub`, `bookforge-store`,
  `bookforge-llm`, `bookforge-pdf`, and `bookforge-cli`.
- `69ed0c1` is the committed v1.7 P0/P1 PDF ingestion work:
  poppler discovery, `bookforge convert`, PDF XML parsing,
  deterministic text reconstruction, synthetic EPUB output, and reports.
- The uncommitted v1.7 P2/P3 media work was lost from the main
  worktree. In particular, `crates/bookforge-pdf/src/media.rs` is not
  present, and the tracked PDF crate files are back to the committed
  P0/P1 state.
- The P3.5 roadmap hardening item has been restored in
  `docs/ROADMAP.md`.
- Current uncommitted main-worktree files after damage control:
  `.gitignore`, `HANDOFF.md`, and `docs/ROADMAP.md`.

## 2. v1.5 release status

The first release candidate in:

```text
tmp/release-v1.5
```

was stale. It was based on `22eaec1` and excluded the GitHub-merged
v1.7 PDF P0/P1 commit, so it must not be used for release decisions.

The real release prep was done from GitHub `origin/main` in:

```text
tmp/release-v1.5-main
```

Release branch/PR/tag:

- release branch: `release-v1.5-from-main`;
- release prep commit: `2a92e789`;
- merged GitHub commit: `1ce614b`;
- PR: <https://github.com/JunjoSick/bookforge/pull/19>;
- tag: `v1.5.0`;
- release: <https://github.com/JunjoSick/bookforge/releases/tag/v1.5.0>.

Validation passed on the corrected release worktree:

```powershell
cargo fmt --all --check
$env:RUSTFLAGS = '-D warnings'; cargo test --workspace --locked
$env:RUSTFLAGS = '-D warnings'; cargo clippy --all-targets --all-features -- -A clippy::too_many_arguments -D warnings
$env:RUSTFLAGS = '-D warnings'; cargo build --release --locked
```

GitHub CI also passed for PR #19 and for the merged `main` commit,
including MSRV `1.88.0`. The tag-triggered Release workflow completed:
plan, all platform artifacts, global artifacts, host, GitHub Release
creation, and announce all succeeded.

crates.io publication completed in dependency order:

1. `bookforge-core v1.5.0`
2. `bookforge-epub v1.5.0`
3. `bookforge-store v1.5.0`
4. `bookforge-llm v1.5.0`
5. `bookforge-pdf v1.5.0`
6. `bookforge-cli v1.5.0`

## 3. Damage-control backups

The latest snapshot is:

```text
tmp/damage-control/20260613-180731
```

It contains status/log/diff captures for the main and release worktrees.
The first backup attempt at `tmp/damage-control/20260613-180656` was
partial and recursively copied some backup files into itself; keep it
only as incident evidence.

## 4. Historical PDF media work that was lost

Before the cleanup, the uncommitted v1.7 P2/P3 work reportedly added:

- figure/table/equation preservation via a new
  `crates/bookforge-pdf/src/media.rs`;
- media-capable model types including `DocBlock`, `Rect`, `PdfImage`,
  `EpubAsset`, `MediaKind`, and `PositionedDocBlock`;
- parser support for `<image>` positions from poppler XML;
- reconstruction that emitted positioned blocks and captions;
- `pdfimages` / `pdftoppm` discovery and page crop extraction;
- EPUB writer support for image assets and
  `<figure><img/><figcaption>`;
- conversion-report media counts, preserved raster chars, and accounted
  coverage;
- README and `.gitignore` updates.

Historical BERT results before deletion:

- `tmp/pdfs/bert.converted.epub` / `bert.convert.json`;
- 16 pages, 5 figures, 7 table crops, 50 equation crops;
- 62 raster crops total;
- 100% accounted coverage;
- mock translate preserved 62/62 image resources;
- real DeepSeek run produced `tmp/pdfs/bert.deepseek.it.epub`;
- DeepSeek run succeeded 2/2, failed 0, needs-review 0, 9 requests OK;
- estimated cost about `$0.057480`;
- nonfatal QA warnings: missing preserved numbers and repetition.

Those artifacts and source files are not currently present in the
workspace.

## 5. Known PDF layout defects to reimplement against

The BERT visual read-through exposed:

1. Figure crops can include the original English caption strip while the
   EPUB also emits a translated `<figcaption>`.
2. Media blocks can interrupt paragraph continuation, leaving orphan
   starts such as `ing ...`.
3. Equation/table crop detection was too broad and rasterized ordinary
   model-parameter prose fragments such as `(L=12, H=768, ...)`.

These are now recorded as P3.5 in `docs/ROADMAP.md`.

## 6. Next practical steps

1. Optionally sync the local main checkout to `origin/main` once the
   damage-control files are either saved elsewhere or intentionally kept.
2. Reconstruct the lost v1.7 P2/P3 media implementation from the notes
   above or restart that work from the committed P0/P1 baseline.
3. Implement the roadmap gaps that were not part of committed P0/P1:
   `doctor --pdf`, P2/P3 media preservation, P3.5 hardening, and later
   P4 degraded-layout fallback.
