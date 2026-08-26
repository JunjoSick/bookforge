# BookForge — Technical Roadmap, v1.0.1 through v2.6.x

**Document version:** 1.5.0
**Last updated:** 2026-07-21
**Status:** historical roadmap plus active follow-up notes
**Audience:** project maintainer + Claude Code (or any other coding agent) implementing
the milestones below.

> **Current status:** v2.6.0 shipped and was tagged on 2026-07-21. It delivered
> audiobook narration hierarchy, chaptered `.m4b` output by default,
> ElevenLabs consistency controls, cost/quota estimates, WebUI parity, and OCR
> endpoint foundations. v2.6.1 was prepared and merged to `main` on 2026-07-21
> with security-audit remediation and two ElevenLabs fixes, and was tagged and
> released on 2026-07-22.
> This document is kept for architectural invariants, shipped-milestone
> context, and deferred follow-up work. For current user behavior, start
> with `README.md`, `CHANGELOG.md`, and `docs/v2-web-dashboard-plan.md`;
> older milestone sections below are historical unless explicitly marked
> as follow-up.
>
> **Milestone numbers vs. release versions.** The `v1.x` labels below are
> roadmap *milestone names*, frozen when this plan was written; they no
> longer track released product versions (semver `v2.x`), and milestones
> shipped out of order. Read them as feature names. Mapping so far:
> milestone v1.8 → releases v1.8.x (2026-06-20); v2.0 milestone →
> releases v2.0.0–v2.0.3; milestone v1.6 (PDF hardening) → release
> v2.1.0 (2026-07-03); milestone v1.7 (bilingual output) → release
> v2.2.0 (2026-07-04). Reflow and cooperative controls shipped in v2.3.0;
> durable corrections and live reconfiguration shipped in v2.4.0; the initial
> audiobook pipeline shipped in v2.5.0, followed by the v2.5.1 dashboard fix.

---

## 0. How to use this document

This is a sequenced implementation plan, not a feature wishlist. Milestones are
versioned, ordered, and scoped. **Do not skip ahead.** Each milestone produces
artifacts that the next milestone depends on, sometimes architecturally,
sometimes empirically (e.g. v1.2 is informed by what users flag in v1.1's
review UI).

For each milestone, this document specifies:

- **Goal** — one paragraph describing what done looks like in user-visible terms.
- **Architectural rationale** — why this milestone, why now, why this scope.
- **Deliverables** — concrete artifacts the milestone must produce.
- **Schema changes** — SQL, TOML, JSON, file-layout changes.
- **CLI / library surface** — every new or changed flag, command, function.
- **Implementation notes** — guidance, gotchas, design decisions already made.
- **Out of scope (within milestone)** — things that look like they belong here but don't.
- **Acceptance criteria** — testable predicates that must be true to call the milestone done.
- **Estimated effort** — rough size in person-days, assuming one focused developer with AI assistance.
- **Dependencies** — explicit links to prior milestones.

If a detail isn't in this document, the document is wrong or incomplete and
the maintainer should be asked. Do not invent.

---

## 1. Architectural invariants (do not violate)

These are non-negotiable. Any change request that violates one of these is
either misdesigned or signals a need to revisit this section explicitly,
not silently bypass it.

### 1.1 The program owns EPUB structure. The model only ever sees validated JSON prose payloads.

This is the load-bearing invariant of the entire project. The LLM is never
shown raw XHTML, never asked to produce raw XHTML, never asked to repair
raw XHTML. Every translation request is a structured JSON payload of prose
fragments with markers; every response is validated as JSON, parsed, and
reassembled deterministically by `bookforge-epub`.

If the model produces malformed output, the response is rejected and the
segment is retried — **never** sent to a "repair" model. If repair is
genuinely required, that is a bug in `bookforge-core` segmentation or
`bookforge-epub` rebuild logic, and that is where it must be fixed.

### 1.2 Structure reassembly is always deterministic.

There is no fill-LLM. There is no second model "putting things back into
XML". Reassembly is pure code, ideally pure functions over the IR. This
distinguishes BookForge from competitors (notably oomol-lab/epub-translator)
that delegate structure assembly to a low-temperature secondary LLM. That
pattern is an architectural smell, not a feature.

### 1.3 The cache is content-addressable and respects prompt versioning.

A cached translation is reused if and only if `(source_hash, prompt_contract_version,
provider, model, source_language, target_language)` all match. Prompt
versioning has a major/minor split (see §11.3); cache is keyed on major only,
so prose-level prompt revisions don't invalidate cache.

### 1.4 Quality is measured by reader experience, not by feature count.

Every milestone after v1.4 must answer: "does this make the next book
better to read?" If the answer is no, it's not a priority, regardless of how
clever the feature is.

### 1.5 The CLI and JSONL event schema follow semver.

Once a flag or event field is shipped in a v1.x release, it is supported
for the lifetime of v1. Breaking changes go in v2. Additive changes
(new flags, new event types, new JSON fields) are minor-version compatible.

### 1.6 Single-binary distribution is a goal, not a coincidence.

BookForge is Rust because Rust gives us static binaries that run anywhere.
Do not introduce dependencies (RocksDB, embedded V8, JVM components, etc.)
that compromise this. EPUBCheck is allowed as an *external* tool invoked
via `java`, but the BookForge binary itself does not need Java to run.

---

## 2. Roadmap overview

| Version | Theme | Estimated effort | Marketing posture | Status |
|---------|-------|------------------|-------------------|--------|
| v1.0.1 | Snapshot patch | 0.5–1 day | none (silent patch) | shipped |
| v1.1 | Review loop | 5–8 days | minimal (README rewrite, GitHub topics, crates.io publish) | shipped |
| v1.2 | Glossary, manual | 8–12 days | none (development quiet) | shipped |
| v1.2.x | Glossary, auto-extraction | 4–6 days | none | shipped |
| v1.3 | Context + style | 8–12 days | none (explicit "no promotion" rule) | shipped |
| v1.4 | Distribution + writeup | 5–7 days | one technical post, two or three venues; cargo-dist binaries land here | shipped |
| v1.5 | Extraction + scheduling overhaul (shipped scope; see §8 post-ship note) | — | none | shipped 2026-06-12 |
| v1.6 | PDF ingestion hardening (§9) | 8–14 days | release notes; maybe a short writeup if layout reconstruction turns out interesting | **shipped 2026-07-03 as release v2.1.0** |
| v1.7 | Bilingual output (§9b) | 5–8 days | passive (release notes only) | **shipped 2026-07-04 as release v2.2.0** |
| v1.8 | Structural credibility (EPUBCheck + corpus; was the planned v1.5 scope, §8) | 10–14 days | README final rewrite citing corpus | **shipped 2026-06-20** |
| v2.0 | Monitoring UI (`RunState`, `watch`, `--ui tui`, local `serve`) | shipped scope | release notes | shipped; patch line completed at v2.0.3 (2026-07-02) |
| v2.6.0 | Audiobook narration quality & UI parity (§16) | 8–12 days | release notes and updated audiobook guide | **shipped 2026-07-21 as release v2.6.0** |
| v2.6.1 | Security-audit remediation + ElevenLabs fixes | patch release | release notes | **merged 2026-07-21; tag pending** |

Priority note (2026-06): the owner needs PDF translation more than
bilingual output — scientific papers (figures/tables must survive) and
unorthodox-layout books (CCRU-style scans/conversions). PDF therefore becomes
v1.6, while bilingual output moves to v1.7.

Total v1.x roadmap is roughly 45–70 person-days of focused work; in calendar
terms, with a maintainer who has limited evenings and weekends and a
girlfriend whose books are the actual point, plan for 6–10 months realistic.

---

## 3. v1.0.1 — Snapshot patch

### 3.1 Goal

Resume must work even if the original input EPUB has been moved, renamed, or
deleted between job submission and resume time. Currently it doesn't, and
this is a footgun severe enough to warrant a patch release rather than waiting
for v1.1.

### 3.2 Architectural rationale

The current resume flow looks up the input EPUB by its original path,
recorded at submission. If the user reorganizes their library, runs a job
from `~/Downloads/book.epub` and then moves it to `~/Books/`, resume fails.
This is a one-evening fix, but every day it isn't fixed is a day a real job
can be lost. Patch release before v1.1 work begins.

### 3.3 Deliverables

- Snapshot the input EPUB into the job directory at submit time.
- Resume reads from the snapshot by default.
- Existing jobs without snapshots fall back to old behavior with a deprecation warning.

### 3.4 File-layout changes

```
.bookforge/runs/<job-id>/
  input.epub          # snapshot of the source EPUB at submission time
  input.sha256        # hex-encoded sha256 of input.epub
  events.jsonl        # existing
  ...
```

### 3.5 Schema changes

Migration `0002_v1_0_1_input_snapshot.sql` adds two columns to the `jobs`
table:

```sql
ALTER TABLE jobs ADD COLUMN input_snapshot_path TEXT;
ALTER TABLE jobs ADD COLUMN input_sha256 TEXT;
```

Existing rows get NULL in both columns; the resume code path checks for NULL
and falls back to the old `input_path` column with a `tracing::warn!()` line
informing the user that the job predates v1.0.1 and resume may fail if the
file has moved.

### 3.6 CLI surface (no changes)

No new flags. The behavior is silently better.

### 3.7 Implementation notes

- Use a hardlink (`std::fs::hard_link`) where possible to avoid duplicating
  bytes on the same filesystem; fall back to copy if hardlink fails (cross-device,
  permission, etc.). On Windows hardlinks need the privilege; just copy there.
- Compute the sha256 during the copy by streaming, not by reading twice.
- The snapshot is the *source of truth* for resume, retry, and re-validation.
  The original `input_path` is preserved only for diagnostic display.
- Do not store the EPUB bytes in SQLite. File-based snapshot is simpler,
  inspectable, and avoids bloating the DB.

### 3.8 Out of scope (within milestone)

- Compressing or deduplicating snapshots across jobs.
- Garbage-collecting completed-job snapshots (deferred).
- Adding a `bookforge gc` command (probably v2).

### 3.9 Acceptance criteria

1. Submit a translation job, then move/rename the input file.
   `bookforge resume <job-id>` succeeds.
2. Submit a translation job, then delete the input file.
   `bookforge resume <job-id>` succeeds.
3. A job created before this patch (no snapshot) resumes with a deprecation
   warning if the original path still exists; fails with a clear error
   message if it doesn't.
4. `input.sha256` matches the actual sha256 of `input.epub` for all new jobs.
5. Existing tests still pass; one new integration test covers the moved-file scenario.

### 3.10 Effort

0.5–1 day.

---

## 4. v1.1 — Review loop

### 4.1 Goal

After a translation job, the user gets a generated HTML page where every
segment of the source is shown side-by-side with its translation. The user
can flag bad paragraphs interactively. Flagged paragraphs are exported as
JSON and ingested back into BookForge to seed glossary entries (in v1.2)
and to mark segments for retry. This is the **measurement instrument** for
all subsequent quality work.

### 4.2 Architectural rationale

Every quality feature downstream of this milestone (glossary, context,
style, semantic QA) is more useful if there's a structured way for a human
reader to identify what's wrong. Without this, feedback is "this chapter
felt off" — too vague to act on. With this, feedback is per-segment with
typed categories. The glossary in v1.2 will be informed by the kinds of
flags users (initially, you and your girlfriend) actually raise. Build the
instrument before the things you measure.

This is also the v1.x milestone with the highest ratio of user-visible
value per line of code. It's a static HTML file with a JSON sidecar.

### 4.3 Deliverables

- `bookforge review <job-id>` command — generates HTML + JSON artifacts.
- `bookforge review <job-id> --open` flag — opens in default browser.
- `bookforge ingest-flags <job-id> --flags <flags.json>` command —
  ingests user feedback. In v1.1 this just records flags into the SQLite
  store and marks flagged segments for retry; glossary integration lights
  up in v1.2.
- Token-usage breakdown including cache tokens, surfaced in the review
  HTML and in `review.json`.
- README opening rewrite (honest one-paragraph statement of what BookForge
  is, why it exists, who it's for).
- GitHub repository topics set: `epub`, `translation`, `rust`, `llm`, `cli`,
  `openrouter`, `cli-tool`, `ebook`.
- crates.io publish for `bookforge-cli` (and the workspace crates that need
  to be published transitively).
- After a successful `bookforge translate` run, the CLI prints a one-line
  hint pointing to the review command, so users discover the feedback
  loop without reading documentation:
  `Review: bookforge review <job-id> --open`.

(cargo-dist and prebuilt-binary distribution are deferred to v1.4, where
the technical writeup will drive the traffic that justifies the install
path. Until v1.4, users install via `cargo install bookforge-cli` from
crates.io or build from source.)

### 4.4 File-layout additions

```
.bookforge/runs/<job-id>/
  review/
    index.html        # generated, self-contained, no external assets
    review.json       # structured segment data, written by `bookforge review`
    style.css         # default review stylesheet, embedded in index.html
```

`flags.json` is **not** a file BookForge writes to this directory. It is
downloaded by the user from the review HTML via the `Export flags`
button (browsers cannot silently write to a sibling file path on disk).
The user then passes the downloaded file back via
`bookforge ingest-flags <job-id> --flags <path-to-downloaded-flags.json>`.
By convention users will save the download into the review/ directory
themselves, but BookForge does not depend on or enforce that path.

### 4.5 review.json schema

```json
{
  "schema_version": 1,
  "job_id": "01HQXY...",
  "source_language": "English",
  "target_language": "Italian",
  "provider": "openrouter",
  "model": "deepseek/deepseek-v4-flash",
  "generated_at": "2026-05-06T12:34:56Z",
  "source_book_title": "The Treasure Island",
  "source_book_author": "Robert Louis Stevenson",
  "totals": {
    "segments": 412,
    "tokens_input": 248391,
    "tokens_input_cached": 184220,
    "tokens_output": 312044,
    "estimated_cost_usd": 0.1934
  },
  "segments": [
    {
      "segment_id": "seg_0001",
      "chapter_id": "chap_01",
      "chapter_title": "The Old Sea-Dog at the Admiral Benbow",
      "ordinal": 1,
      "source_text": "...",
      "target_text": "...",
      "soft_warnings": [
        {"kind": "length_ratio", "value": 2.31, "threshold": 2.0},
        {"kind": "url_changed", "from": "...", "to": "..."}
      ],
      "tokens": {
        "input": 612,
        "input_cached": 488,
        "output": 740,
        "estimated": true
      },
      "status": "completed"
    }
  ]
}
```

The `tokens.estimated` field is `true` when the segment was part of a
batched request and per-segment values were apportioned (see §4.9 for
the apportionment rule). When `false`, the values are exact. Totals at
the document level are always exact, regardless of per-segment estimation.

### 4.6 flags.json schema

```json
{
  "schema_version": 1,
  "job_id": "01HQXY...",
  "exported_at": "2026-05-06T13:45:00Z",
  "flags": [
    {
      "segment_id": "seg_0042",
      "kind": "name",
      "note": "Character name was rendered inconsistently with chapter 1.",
      "suggested_source": "Long John Silver",
      "suggested_target": "Long John Silver"
    },
    {
      "segment_id": "seg_0107",
      "kind": "register",
      "note": "Too formal — these are pirates speaking informally.",
      "suggested_source": null,
      "suggested_target": null
    },
    {
      "segment_id": "seg_0231",
      "kind": "wrong_translation",
      "note": "Last sentence reverses the meaning of the original.",
      "suggested_source": null,
      "suggested_target": null
    }
  ]
}
```

`kind` is one of: `name` | `register` | `wrong_translation` | `formatting` | `tone` | `other`.

### 4.7 HTML behavior

The review HTML is **self-contained**. No external CDN, no fonts loaded
remotely, no analytics. It can be opened from a USB stick on a flight.
All CSS and JS are inlined. It loads `review.json` from a sibling file
via `fetch('./review.json')`, but if `fetch` fails (e.g. opened via
`file://` and the browser blocks it), it falls back to a JSON blob
embedded in the HTML.

UI:

- Two columns: source on the left, target on the right.
- Synchronized scrolling.
- Each segment has a small "flag" button that opens a popover:
  - select kind (radio)
  - free-text note (textarea)
  - optional suggested source/target text
- A persistent header bar shows: total segments, flagged count, `Export flags` button.
- `Export flags` triggers a download of `flags.json`.
- Flags are persisted in `localStorage` between sessions, keyed by `job_id`.
- Soft warnings on a segment are rendered as small badges (e.g. "length ratio 2.3x")
  with hover tooltips.
- Search box filters segments by source or target text (case-insensitive substring).
- Filter buttons: "all" | "flagged" | "warnings" | "needs review" | "completed".

### 4.8 ingest-flags behavior (v1.1 scope)

```
bookforge ingest-flags <job-id> --flags <flags.json>
```

In v1.1, this command:

1. Validates `flags.json` against the schema (reject with clear error if invalid).
2. Stores each flag into a new SQLite table `segment_flags`.
3. For flags of kind `wrong_translation`: marks the corresponding segment
   as `needs-review` so it can be retried via `bookforge retry <job-id> --only needs-review`.
4. Prints a summary: "Ingested N flags. M segments marked needs-review.
   Glossary integration will be available in v1.2."

Schema:

```sql
CREATE TABLE segment_flags (
  id INTEGER PRIMARY KEY,
  job_id TEXT NOT NULL,
  segment_id TEXT NOT NULL,
  kind TEXT NOT NULL,
  note TEXT,
  suggested_source TEXT,
  suggested_target TEXT,
  ingested_at TEXT NOT NULL,
  consumed INTEGER NOT NULL DEFAULT 0,
  FOREIGN KEY (job_id) REFERENCES jobs(id) ON DELETE CASCADE
);

CREATE INDEX idx_segment_flags_job ON segment_flags(job_id, consumed);
```

`consumed = 1` once the flag has been used by a downstream feature
(glossary import in v1.2, retry in v1.1, etc.). This avoids double-applying
the same flag.

### 4.9 Token-usage breakdown (oomol-lab inspiration)

The provider trait already returns token counts after a request. Extend
the response struct to include cached input tokens where the provider
supports it, and an `estimated` flag for per-segment apportionment:

```rust
pub struct ProviderTokenUsage {
    pub input_tokens: u32,
    pub input_cached_tokens: u32,  // 0 if provider doesn't support
    pub output_tokens: u32,
    pub estimated: bool,            // true when apportioned across batch items
}
```

**Apportionment for batched requests.** Providers report token usage at
the *request* level, not the segment level. When BookForge batches
multiple segment items into a single request (which it does whenever
the request shape allows), per-segment usage cannot be measured exactly.
The apportionment rule:

1. Compute each batched item's source-token weight `w_i` (using the
   tokenizer for the active model, or a uniform character-count
   approximation if the tokenizer is unavailable).
2. For each item, attribute `input_tokens * (w_i / sum(w))`,
   `input_cached_tokens * (w_i / sum(w))`, and
   `output_tokens * (w_i / sum(w))`, rounded to integers.
3. The last item in the batch absorbs any rounding remainder so that
   the per-item sums match the request totals exactly (no token leakage).
4. All per-segment values produced this way are stored with
   `estimated = true`. Single-segment requests store `estimated = false`.

**Totals are exact even when per-segment values are estimates.** Because
the apportionment rule preserves request-level sums, the totals in
`review.json` are exact at the request level even when individual
segment values are approximations. Distinguish these two cases in the
review UI: per-segment numbers with `estimated: true` are rendered with
a small "≈" indicator; totals are rendered without the indicator.

Schema (per-segment usage stored in checkpoint store):

```sql
ALTER TABLE segments ADD COLUMN tokens_input INTEGER;
ALTER TABLE segments ADD COLUMN tokens_input_cached INTEGER;
ALTER TABLE segments ADD COLUMN tokens_output INTEGER;
ALTER TABLE segments ADD COLUMN tokens_estimated INTEGER NOT NULL DEFAULT 0;
```

Surface in `review.json` (per-segment) and in the review HTML header bar
(totals). The per-segment `tokens` block in `review.json` includes the
`estimated` field:

```json
"tokens": {
  "input": 612,
  "input_cached": 488,
  "output": 740,
  "estimated": true
}
```

### 4.10 README opening rewrite

Replace the current `# Bookforge` opening paragraph with something like:

```markdown
# BookForge

BookForge is the EPUB translation engine that keeps the LLM away from
your document structure. It parses EPUBs into validated JSON payloads,
checkpoints every segment, preserves markup/footnotes/links, and rebuilds
EPUBCheck-clean books.

I built this to translate books for my partner. It's MIT-licensed in
case it's useful to you.
```

Keep the rest of the README operational. Do not yet add a comparison
matrix or demo video — that's v1.4 territory.

### 4.11 CLI surface additions

```
bookforge review <job-id> [--open] [--out <dir>]
bookforge ingest-flags <job-id> --flags <path-to-flags.json>
```

Translate command extended with telemetry surfacing (no new flags, just
better data in `review.json` and events) and a post-success hint line:
`Review: bookforge review <job-id> --open`.

### 4.12 Out of scope (within milestone)

- Glossary table or any glossary logic — v1.2.
- Any LLM-based feedback ingestion (e.g. "use a model to extract glossary
  candidates from flags") — v1.2.x at earliest.
- A web server. The review HTML is static and file-based. Do not add
  a server even if it would be slightly more convenient.
- A diff view between source and target. Not the right model for this
  task; we're showing source vs. translation, not source vs. edited source.
- **In-place editing of translated text.** The review UI accepts flags
  (typed feedback with optional suggested target). It does not allow
  the user to edit the translation directly in the UI. Flags drive
  retries and glossary updates; direct edits would require a different
  reconciliation model and are explicitly out of scope for v1.1.
- Distribution via cargo-dist / Homebrew / AUR / winget — moved to v1.4.

### 4.13 Privacy note

Review artifacts (`review/index.html`, `review/review.json`, and any
`flags.json` the user exports) contain the **full source text and full
translated text** of the book. Treat them as private user data.
BookForge does not transmit them anywhere; they live in
`.bookforge/runs/<job-id>/review/` on the local filesystem. Users
sharing a machine should be aware that other users with read access to
that directory can read the book contents.

This note is reproduced in the README (in the v1.5 final rewrite) and
should be surfaced in the review HTML itself as a small footer line:
"This page contains the full text of your book. Treat as private."

### 4.14 Acceptance criteria

1. After a successful translation job, `bookforge review <job-id>` produces
   `review/index.html` and `review/review.json`.
2. After a successful `bookforge translate`, the CLI prints the review
   hint line.
3. Opening `review/index.html` in a current browser (Firefox, Chrome, Safari)
   shows source and target side-by-side, with synchronized scrolling.
4. Flagging segments persists across browser refresh via `localStorage`.
5. `Export flags` downloads a valid `flags.json` matching §4.6.
6. `bookforge ingest-flags <job-id> --flags flags.json` validates the
   file, populates `segment_flags`, and marks `wrong_translation` flags
   as needing retry.
7. `bookforge retry <job-id> --only needs-review` retries the marked segments.
8. Token usage totals in `review.json` are exact and equal the sum of
   per-segment values stored in the SQLite checkpoint store. Per-segment
   values for batched requests are marked `estimated: true`; for single-
   segment requests, `estimated: false`.
9. README opening paragraph is rewritten per §4.10.
10. The package is published on crates.io and `cargo install bookforge-cli`
    works from a clean machine with Rust installed.

### 4.15 Effort

5–8 days. The HTML/JS is the largest single chunk; budget 2–3 days for
it alone. (Reduced from 6–10 days because cargo-dist work is no longer
in this milestone.)

### 4.16 Dependencies

v1.0.1 must be merged. The snapshot file is referenced in `review.json`
metadata so the review can be regenerated from a moved/renamed source.

---

## 5. v1.2 — Glossary (manual)

### 5.1 Goal

Users (you, the maintainer; later, anyone else) can define a glossary
of terms — proper nouns, invented terminology, register decisions —
that BookForge will inject into every segment translation prompt.
Glossary entries can be book-scoped, series-scoped, or global, and
the cross-book persistence is what lets a multi-book series stay
consistent without manual re-seeding.

The review UI lights up: it now highlights segments where the translation
fails to honor an active glossary entry.

**Auto-extraction is explicitly out of scope for v1.2.** It is a quality-
of-life feature deferred to v1.2.x once we've used the manual glossary
on at least two real books and learned what kinds of terms actually matter.

### 5.2 Architectural rationale

The current cache reuses translations on identity match. The same character
name in segment 1 and segment 401 translates independently and may drift.
A glossary is the simplest, cheapest, most controllable mechanism to lock
down terminology and register choices. Auto-extraction is more sophisticated
but not on the critical path; it's a faster way to seed the manual structure
once the manual structure exists.

Series scoping is the upgrade over what GPT-5.5's roadmap suggested. If
you're translating a series for your girlfriend, "Aragorn" must translate
the same way in book three as in book one. Series scoping is one extra
column and a `--series` flag and it changes the system from "translates a
book" to "translates her books."

### 5.3 Deliverables

- `glossary_terms` table in SQLite.
- TOML import/export format (§5.5).
- CLI commands for glossary management (§5.7).
- Prompt injection during translation, token-budgeted (§5.8). Two
  render formats — structured JSON and prose bullets — selectable via
  `--glossary-format`; both ship in v1.2 so we can A/B which the model
  honors better.
- Review UI updated to highlight glossary mismatches (§5.9).
- `--prompt-extra` flag for ad-hoc instructions (oomol-lab inspiration, §5.10).
- ingest-flags from v1.1 lights up: flags of kind `name` with
  `suggested_target` are imported as `user_seeded` glossary entries.

### 5.4 Schema

```sql
CREATE TABLE glossary_terms (
  id INTEGER PRIMARY KEY,
  scope_kind TEXT NOT NULL CHECK(scope_kind IN ('global', 'series', 'book')),
  scope_id TEXT,                         -- NULL for global
  source_text TEXT NOT NULL,
  target_text TEXT NOT NULL,
  category TEXT NOT NULL CHECK(category IN
    ('person', 'place', 'object', 'invented', 'style', 'phrase', 'other')),
  notes TEXT,
  case_sensitive INTEGER NOT NULL DEFAULT 0,
  always_active INTEGER NOT NULL DEFAULT 0,  -- inject every segment regardless of match
  status TEXT NOT NULL CHECK(status IN
    ('user_seeded', 'auto_candidate', 'accepted', 'rejected')) DEFAULT 'user_seeded',
  source_language TEXT NOT NULL,
  target_language TEXT NOT NULL,
  source_count INTEGER DEFAULT 0,        -- frequency in source corpus, populated by import
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE(scope_kind, scope_id, source_text, source_language, target_language)
);

CREATE INDEX idx_glossary_lookup
  ON glossary_terms(source_language, target_language, scope_kind, scope_id, status);
```

`status` semantics:

- `user_seeded`: human added it, treat as authoritative.
- `auto_candidate`: extracted programmatically (v1.2.x), pending human review.
- `accepted`: was `auto_candidate`, human approved it.
- `rejected`: was `auto_candidate`, human rejected it. Kept in DB to avoid
  re-suggesting the same terms.

Only `user_seeded` and `accepted` entries are injected into prompts.

`always_active` semantics: when set to 1, the entry is injected into
every segment prompt regardless of whether its `source_text` appears
in the segment's source. Default is 0 (entries are only injected when
matched by the segment-presence check or selected by the high-frequency
anchor rule in §5.8). Use `always_active = 1` for global stylistic
constraints that must hold throughout the book — e.g. a register decision
("always use 'tu' in dialogue between hobbits") or a persistent translation
policy ("translate all currency in pounds, not euros"). Do **not** set
`always_active = 1` on broad style or phrase entries unintentionally; it
multiplies prompt token cost across every segment.

### 5.5 TOML format

```toml
[meta]
schema_version = 1
source_language = "English"
target_language = "Italian"

[meta.scope]
kind = "series"           # global | series | book
id   = "lord-of-the-rings"

[[term]]
source = "Aragorn"
target = "Aragorn"
category = "person"
case_sensitive = true
notes = "Preserve as-is; do not transliterate or domesticate."

[[term]]
source = "the One Ring"
target = "l'Unico Anello"
category = "object"
case_sensitive = false

[[term]]
source = "you"
target = "tu"
category = "style"
always_active = true
notes = "Default to informal tu in dialogue between hobbits and friends."
```

A single TOML file represents a single (scope, source_lang, target_lang)
tuple. A series TOML can be loaded alongside a book TOML; entries are
merged with book-scope winning over series-scope winning over global-scope.

The `always_active` field is optional; when omitted it defaults to `false`.
See §5.8 for the prompt-injection ranking that uses it.

### 5.6 Glossary file conventions on disk

By convention, users keep glossaries co-located with their books:

```
~/Books/lord-of-the-rings/
  fellowship.epub
  two-towers.epub
  return-of-the-king.epub
  glossary.series.toml          # series-scoped, applies to all three
  glossary.fellowship.toml      # book-scoped, applies only to fellowship
  style.toml                    # v1.3 territory
```

BookForge does not enforce this layout. The user passes `--glossary`
(repeatable) and BookForge merges them.

### 5.7 CLI surface

```
bookforge glossary list [--book <id>] [--series <id>] [--language <pair>]
bookforge glossary add "<source>" "<target>" \
    --category <kind> \
    [--scope global|series|book] [--scope-id <id>] \
    [--source-lang <lang>] [--target-lang <lang>] \
    [--case-sensitive] [--always-active] [--notes "<text>"]
bookforge glossary remove <id>
bookforge glossary clear --scope <kind> --scope-id <id>
bookforge glossary import <file.toml>
bookforge glossary export <file.toml> [--scope <kind>] [--scope-id <id>]
```

Translate command additions:

```
bookforge translate book.epub \
    --target Italian \
    --provider openrouter \
    --model deepseek/deepseek-v4-flash \
    --book-id "fellowship" \
    --series-id "lord-of-the-rings" \
    --glossary ~/Books/lord-of-the-rings/glossary.series.toml \
    --glossary ~/Books/lord-of-the-rings/glossary.fellowship.toml \
    --glossary-budget-tokens 800 \
    --glossary-format json \
    --prompt-extra "Maintain a literary register typical of Tolkien translation."
```

`--book-id` and `--series-id` associate the job with scope identifiers,
which become the default scope for `bookforge glossary add` if invoked
during the same session.

`--glossary-format` accepts `json` (default) or `prose`. Both inject the
same selected entries; only the rendered shape differs. See §5.8.

### 5.8 Prompt injection (token-budgeted)

The active glossary at translate time is the merged set of all entries
with status `user_seeded` or `accepted` matching the active `(source_lang,
target_lang)` and any of the active scopes (job's book_id, series_id, or global).

**Selection rules.** For each segment, the prompt injects entries selected
by the following rules, in this priority order:

1. **Segment-matched entries.** Entries whose `source_text` appears
   verbatim in the segment's source text (case-sensitive or insensitive
   per the entry's `case_sensitive` field). Highest priority — these
   are always relevant to the current segment.

2. **`always_active` entries.** Entries with `always_active = 1` and
   status `user_seeded` or `accepted`. These are injected into every
   segment regardless of segment-presence, because they encode global
   constraints (register decisions, persistent translation policies)
   that must hold throughout the book.

3. **Recently-active entries.** Entries that were segment-matched in
   any of the previous N segments within the same chapter (default
   N=5). This catches inflected forms in target languages with rich
   morphology — once "l'Unico Anello" matched in segment 14, it stays
   as a soft anchor through segment 19 even if the substring doesn't
   match in segments 15–18.

4. **High-frequency proper-noun anchors.** Entries with status
   `user_seeded` or `accepted`, **restricted to categories
   `person` / `place` / `object` / `invented`**, with `source_count`
   above a threshold (default: top 20 by `source_count` for the active
   scopes). These are persistent character/place names that should
   stay consistent across the whole book even when they don't appear
   in the current segment's source text.

**Important guardrail.** The high-frequency anchor rule (priority 4)
is restricted to proper-noun categories. Entries with category `style`,
`phrase`, or `other` are **never** injected via the high-frequency rule.
If the user wants a style or phrase entry to apply to every segment,
they must mark it `always_active = 1` explicitly. This prevents broad
stylistic constraints from polluting every prompt unintentionally and
multiplying token cost.

**Budget enforcement.** After ranking, entries are serialized in priority
order and truncated at the token budget (default 800 tokens, configurable
via `--glossary-budget-tokens`). A `tracing::warn!()` line fires if
truncation drops any `user_seeded` or `always_active` entries.

**Post-ship note (2026-07-03).** Ranking rules 3 and 4 (recently-active,
high-frequency anchors) were specified before any usage evidence existed,
and nothing instruments whether they ever fire usefully. Before extending
this machinery further, add counters (per rule: entries injected /
entries honored in output) to a real translation run and check the data;
if rules 3–4 contribute nothing measurable, simplify rather than extend.

**Token estimator.** v1.2 used a conservative `chars / 3` heuristic
(rounded up) instead of a real BPE tokenizer, and later a dominant-class
case weighting; both are retired. The audit-2026-08 wave replaced every
estimate site with one script-aware proportional weighting in
`bookforge_core::token_estimate` (unspaced CJK scripts ≈ 1 char/token,
everything else ≈ 4 chars/token). A real tokenizer (`tiktoken-rs` or
per-provider equivalent) remains deferred until measurements show the
heuristic misses materially on real books.
once style sheets land and per-segment token accounting becomes a more
load-bearing concern. Code carries a `// TODO(v1.3): real tokenizer`
marker.

**Render format (selectable).** The injected block has two shapes,
selected per-job via `--glossary-format`. Both inject the same selected
entries; we ship both so users can A/B which the model honors better in
their language pair and against their model. The format choice is
hashed into the segment cache namespace so switching formats does not
silently mix cached translations from different shapes.

`--glossary-format json` (default) populates the existing
`{{glossary_json}}` template placeholder with a structured array:

```json
[
  {"source":"Aragorn","target":"Aragorn","category":"person"},
  {"source":"the One Ring","target":"l'Unico Anello","category":"object"},
  {"source":"you","target":"tu","category":"style",
   "note":"informal in hobbit dialogue"}
]
```

`--glossary-format prose` populates a sibling `{{glossary_block_prose}}`
placeholder with a human-readable bullet list:

```
Active glossary constraints (must be honored):
- "Aragorn" → "Aragorn" (person, preserve)
- "the One Ring" → "l'Unico Anello" (object)
- For dialogue between hobbits: prefer informal "tu" over "Lei"

Active stylistic instructions:
<contents of --prompt-extra, if any>
```

This block is placed **before** the JSON payload to translate, in the
system or user message depending on the provider's preferred prompt
shape (defined in the provider preset).

### 5.9 Review UI: glossary mismatch highlighting

For each completed segment, after translation:

1. Identify glossary entries whose `source_text` appears in `segment.source_text`.
2. For each such entry, check whether `entry.target_text` appears in
   `segment.target_text` (case-insensitive by default, case-sensitive if entry says so).
3. If the source term appears but the target term does not, emit a
   soft warning of kind `glossary_mismatch`:

```json
{"kind": "glossary_mismatch", "term_id": 42, "source": "Aragorn",
 "expected_target": "Aragorn", "found_target": null}
```

The review HTML renders these as red badges with the expected vs. actual.
This is the highest-signal soft warning the system produces; users will
flag many of these as `name` flags, which feed back into the glossary.

Important: glossary mismatch is a soft warning, not a hard failure.
Italian morphology means "l'Unico Anello" might appear as "dell'Unico
Anello" in genitive position. The check is best-effort; the user is the
arbiter via the review UI.

### 5.10 --prompt-extra flag

Borrowed from oomol-lab. A free-text string that the user passes at
translate time to inject ad-hoc instructions. Example uses:

```
--prompt-extra "Use formal register throughout; this is a 19th-century
historical novel."

--prompt-extra "Preserve all technical terminology in English (this is a
computer science textbook)."

--prompt-extra "Translate all dialogue using regional Roman dialect
where the original uses Cockney."
```

This is a low-tech escape valve before the proper style sheet system
in v1.3. Once style sheets land, `--prompt-extra` remains as a quick
override; the style sheet is the durable mechanism.

### 5.11 ingest-flags v1.2 upgrade

When ingesting `flags.json`:

- Flags of kind `name` with `suggested_target` non-null:
  add to glossary with `category = 'person'` (or `'place'`/`'object'`
  if the user picked those) and `status = 'user_seeded'`.
  Default scope is the job's book_id; user can override via
  `bookforge ingest-flags ... --default-scope series`.
- Flags of kind `register`: add to glossary with `category = 'style'`,
  `source_text` set to a placeholder if no `suggested_source` was given.
- All other kinds: stored in `segment_flags` as before, used for retry.

### 5.12 Out of scope (within milestone)

- **Auto-extraction of glossary candidates from source text** — deferred
  to v1.2.x. The manual glossary works without it; auto-extraction is
  an accelerator, not a prerequisite.
- Fuzzy translation memory (n-gram similarity matching of segments).
  This is v2 territory.
- Glossary versioning / history / git-style diff. Not needed yet.
- Cross-language glossary (e.g. one TOML covering EN→IT and EN→FR
  simultaneously). Files are per-language-pair.
- An LLM pass to validate glossary consistency. Not needed; deterministic
  substring check is enough as a soft warning.

### 5.13 Acceptance criteria

1. `bookforge glossary import glossary.toml` populates `glossary_terms`
   with the right scope, status, and language fields.
2. `bookforge glossary export glossary.toml --scope book --scope-id X`
   produces a TOML that, when re-imported, results in identical DB state.
3. A translation job with `--glossary <file>` injects the active terms
   into every segment prompt up to the token budget.
4. Glossary mismatches show up as red badges in the review HTML.
5. Flagging a segment with kind `name` and a `suggested_target`, then
   running `ingest-flags`, results in a new `user_seeded` glossary entry.
6. Translating the same book twice with the same glossary produces
   bit-identical output (deterministic given identical model output).
7. `--prompt-extra "..."` is preserved verbatim in the segment prompts
   sent to the provider (verifiable via the mock provider's recorded
   request log in tests).
8. Translating a book without any `--glossary` flag works exactly as
   in v1.1 (no regressions).
9. Switching `--glossary-format` between `json` and `prose` for the same
   `(book, glossary)` pair produces a different cache namespace and
   re-translates rather than reusing the prior format's cache.

**Cache compatibility note.** v1.2 bumps `CACHE_KEY_SCHEMA_VERSION` from
1 to 2 unconditionally so that glossary content and format always factor
into the cache key. The first v1.2 run on a v1.1 book will re-translate
even with no `--glossary`. This one-time cost is preferred over the
footgun where adding one term mid-job silently mixes glossary-aware and
glossary-blind cached segments.

### 5.14 Effort

8–12 days. Token-budgeted prompt injection with sensible ranking is the
hardest part; budget 3 days for that alone.

### 5.15 Dependencies

v1.1 (review UI must exist for mismatch highlighting; ingest-flags must
exist as a stub). v1.0.1 (input snapshot is referenced when re-translating
with glossary changes).

---

## 5b. v1.2.x — Glossary auto-extraction (point release)

Ship after v1.2 has been used on at least two real books. Goal is to
accelerate seeding new books from existing corpus.

### 5b.1 Approach

Run a pass over the source EPUB:

1. Tokenize using a basic word-boundary regex, preserving capitalization.
2. Identify candidate terms by:
   a. Capitalized words appearing more than 3 times that aren't in a
      common-word list for the source language.
   b. Multi-word capitalized sequences ("New York", "Mount Doom").
   c. Quoted-italic phrases (often invented terminology).
3. For each candidate, store `auto_candidate` rows with `source_count`.
4. CLI: `bookforge glossary review-candidates <book-id>` opens a
   simple TUI (or, more conservatively, prints a numbered list and accepts
   `accept N` / `reject N` / `set N "Italian translation"` commands).

The bar for `auto_candidate` quality is "saves the user 10 minutes of
manual seeding," not "perfect extraction." False positives are fine if
they're cheap to reject.

### 5b.2 Acceptance criteria

1. Auto-extraction runs in under 30 seconds on a 300-page novel.
2. At least 80% of capitalized proper nouns in a known test EPUB are
   surfaced as candidates.
3. Reviewing candidates is faster than typing them by hand from scratch
   (subjective, but verifiable on a real book).

### 5b.3 Effort

4–6 days. Mostly heuristics tuning.

---

## 6. v1.3 — Context + style

### 6.1 Goal

Translation quality improves dramatically when the model has access to
(a) what came before in the same chapter and (b) consistent stylistic
guidance. This milestone adds both, conservatively.

### 6.2 Architectural rationale

Sliding context catches the failure modes that glossary alone can't:
pronoun resolution across paragraphs, narrative voice consistency,
register drift across long chapters. Style sheets formalize the per-
book/per-series stylistic decisions that `--prompt-extra` was a thin
proxy for.

The "conservative" framing matters: naive context injection burns tokens
and risks contamination (a model seeing "previous: bad translation"
sometimes regresses toward it). Defaults must be safe.

### 6.3 Deliverables

- Sliding context in segment prompts, configurable.
- Style sheet system (TOML, per-book or per-series).
- Per-book entity sheet (manually maintained extension of glossary).
- New CLI flags for context and style.

### 6.4 Sliding context

Default behavior:

- Inject the previous **3** segments' source-and-target pairs.
- Scope: same chapter only. Crossing chapter boundaries is opt-in.
- Hard token cap: **1200** tokens. If the previous 3 don't fit, drop the
  oldest first.
- Never include segments with status `failed` or `needs_review` as context.
  These segments often have errors and using them as context contaminates
  the next segment.
- Context is wrapped in unambiguous delimiters in the prompt:

```
=== Context (already translated, do not retranslate) ===
Source: <previous source 1>
Target: <previous target 1>
---
Source: <previous source 2>
Target: <previous target 2>
=== End context ===

=== Translate now ===
<JSON payload>
```

CLI:

```
--context-window <N>            # default 3, 0 disables
--context-budget-tokens <N>     # default 1200
--context-scope chapter|book    # default chapter
```

Implementation note: sliding context interacts with concurrency. If
segments translate in parallel, the "previous N" must be drawn from
the most-recently-completed segments in the canonical order, not the
order of completion. Maintain a "completion fence": segment N's context
is only available once segments N-1, N-2, N-3 are all completed.
Practical effect: concurrency is per-chapter, not cross-chapter.
This is a small but real concurrency reduction; document it.

### 6.5 Style sheet (TOML, per book or series)

```toml
[meta]
schema_version = 1
source_language = "English"
target_language = "Italian"

[meta.scope]
kind = "series"
id = "lord-of-the-rings"

[register]
narration = "literary"            # casual | neutral | literary | formal
narration_tense = "passato_remoto" # passato_remoto | passato_prossimo | mixed
dialogue_default = "tu"            # tu | Lei | voi | mixed
loanword_policy = "translate_when_natural"
                                  # preserve | translate_when_natural | always_translate

[voice]
narrator_register = "elevated"
preserve_anglicisms = false
target_audience = "adult literary reader"
gender_of_unspecified_narrator = "m"

[free_text]
instructions = """
Maintain a literary register typical of mid-20th-century Italian
fiction translation. Prefer passato remoto for narrated past tense
and passato prossimo for direct character thought. Avoid loanwords
where Italian equivalents are natural. Treat dialogue between hobbit
friends as informal (tu); dialogue with elves and lords as formal (Lei).
"""

[do_not]
translate_terms = ["mithril", "lembas", "Eru"]   # treat as proper nouns
preserve_punctuation = ["—", "…"]                 # don't normalize these
```

Style sheets are merged at translate time with the same precedence as
glossaries: book > series > global. The merged style sheet is rendered
as a prompt block:

```
=== Active style guide ===
Register: literary, narration in passato remoto.
Dialogue default: informal (tu).
Loanword policy: translate when natural Italian equivalent exists.
Narrator voice: elevated. Preserve em-dashes and ellipses.

Custom instructions:
<contents of [free_text].instructions>
=== End style guide ===
```

CLI:

```
--style <file.toml>               # repeatable
```

Several style sheets can be active simultaneously (e.g. one for the series,
one for the book); they merge book > series. This mirrors the glossary
file model.

### 6.6 Entity sheet (manually maintained)

Entities are a structured extension of the glossary specifically for
characters and named entities whose grammatical-gender state matters
in the target language. Italian, French, Spanish, German, etc. all
need this; English doesn't.

```toml
[meta]
schema_version = 1
source_language = "English"
target_language = "Italian"

[meta.scope]
kind = "book"
id = "fellowship"

[[entity]]
source_name = "Galadriel"
target_name = "Galadriel"
gender_target = "f"
role = "elf-queen"
notes = "Address as 'Lady Galadriel' / 'Signora Galadriel' in formal speech."

[[entity]]
source_name = "Boromir"
target_name = "Boromir"
gender_target = "m"
role = "warrior"

[[entity]]
source_name = "the Ring"
target_name = "l'Anello"
gender_target = "m"
role = "object"
```

The entity sheet is injected into prompts as a "grammatical agreement
table":

```
=== Entity grammatical agreement (use this for adjective/article concord) ===
- Galadriel: feminine
- Boromir: masculine
- l'Anello (the Ring): masculine
=== End ===
```

This catches the failure mode where the model translates "she said,
looking at the Ring" with feminine agreement on the wrong noun. With
the table, the model has explicit guidance.

CLI:

```
--entities <file.toml>            # repeatable
```

Implementation note: entities are stored in a separate table from
glossary terms because they have different fields and different
prompt-injection format, but they share the scope model.

```sql
CREATE TABLE entities (
  id INTEGER PRIMARY KEY,
  scope_kind TEXT NOT NULL CHECK(scope_kind IN ('global', 'series', 'book')),
  scope_id TEXT,
  source_name TEXT NOT NULL,
  target_name TEXT NOT NULL,
  gender_target TEXT,           -- 'm' | 'f' | 'n' | NULL
  role TEXT,
  notes TEXT,
  source_language TEXT NOT NULL,
  target_language TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE(scope_kind, scope_id, source_name, source_language, target_language)
);
```

### 6.7 Marketing posture for v1.3

**No promotion.** This is the milestone GPT-5.5's roadmap correctly
flagged as quiet development. The previous milestones were either
infrastructure (v1.0.1, v1.1) or visible feature (v1.2 glossary); v1.3
is depth work that's mostly invisible until you read a translated
book and notice it's better. Don't post about it. Save the announcement
for the v1.4 writeup.

### 6.8 Out of scope (within milestone)

- Coreference resolution (programmatic). The entity sheet is manually
  maintained. Automating it is v2.
- Style sheet validation against a target-language grammar checker.
  Out of scope; the style sheet is advisory to the model, not a hard checker.
- Multiple parallel context strategies (e.g. "context from prior chapters'
  endings"). Not yet justified by data.
- A graphical style-sheet editor.

### 6.9 Acceptance criteria

1. Translating a 100-segment chapter with `--context-window 3 --context-scope chapter`
   includes the previous 3 segments' source-target pairs in the prompt
   for segments 4 through 100, scoped to the same chapter.
2. A failed segment (status `needs_review`) is never injected as context
   into a subsequent segment.
3. A style sheet with `[register].dialogue_default = "tu"` results in
   the rendered style block being part of the prompt, verifiable via
   the mock provider's request log.
4. Entity gender entries are rendered into the prompt's grammatical
   agreement block.
5. Translating the same book with vs. without context shows visibly
   improved pronoun consistency (subjective; verify on at least one
   real book before declaring v1.3 done).
6. No regressions in v1.1 / v1.2 functionality.

### 6.10 Effort

8–12 days.

### 6.11 Dependencies

v1.2 (glossary file model is the precedent for style and entity files;
prompt-injection plumbing is shared).

---

## 7. v1.4 — Distribution + writeup

### 7.1 Goal

Lower install friction to near zero for non-Rust users (Homebrew, AUR).
Audit and lower MSRV if the bleeding-edge toolchain isn't paying for itself.
Publish one technical architecture writeup in two or three appropriate venues.

### 7.2 Architectural rationale

By v1.3, BookForge is doing real, distinguishable work that's worth
telling people about. The writeup is the only intentional marketing
event in the entire roadmap. Distribution is its prerequisite: the
post drives traffic; if the install path is `cargo install` only, most
of that traffic bounces.

### 7.3 Deliverables

- cargo-dist GitHub release configuration; first release with prebuilt
  binaries for macOS x86_64+arm64, Linux x86_64+arm64, Windows x86_64.
  (Originally drafted for v1.1 but moved here, where the writeup justifies
  the distribution surface.)
- Homebrew tap: `brew tap junjosick/bookforge && brew install bookforge`.
- AUR package: `bookforge` and/or `bookforge-bin`.
- (Optional) Scoop bucket for Windows.
- (Optional) Winget manifest.
- MSRV audit and possible reduction.
- One writeup post (~2000–4000 words) on BookForge architecture.
- CONTRIBUTING.md and an issue template — minimal, honest.

### 7.4 cargo-dist setup

Add `cargo-dist.toml` at workspace root. Configure for:

- targets: `x86_64-apple-darwin`, `aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`,
  `aarch64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`.
- installers: `shell` (curl pipe), `powershell` for Windows.
- archives: `.tar.xz` for unix, `.zip` for windows.
- GitHub Actions workflow generated; commit it.

First binary release as `v1.4.0` after all v1.4 work is merged. Tag,
push, let CI build artifacts and publish them as a GitHub release.
The Homebrew tap and AUR PKGBUILD (§7.5, §7.6) reference these
release artifacts by URL and sha256.

### 7.5 Homebrew tap

Create repository `junjosick/homebrew-bookforge` with:

```ruby
# Formula/bookforge.rb
class Bookforge < Formula
  desc "EPUB translation engine that keeps the LLM away from your document structure"
  homepage "https://github.com/JunjoSick/bookforge"
  version "1.4.0"

  on_macos do
    on_arm do
      url "https://github.com/JunjoSick/bookforge/releases/download/v1.4.0/bookforge-aarch64-apple-darwin.tar.xz"
      sha256 "..."
    end
    on_intel do
      url "https://github.com/JunjoSick/bookforge/releases/download/v1.4.0/bookforge-x86_64-apple-darwin.tar.xz"
      sha256 "..."
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/JunjoSick/bookforge/releases/download/v1.4.0/bookforge-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "..."
    end
    on_intel do
      url "https://github.com/JunjoSick/bookforge/releases/download/v1.4.0/bookforge-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "..."
    end
  end

  def install
    bin.install "bookforge"
  end

  test do
    system "#{bin}/bookforge", "--version"
  end
end
```

cargo-dist can generate this automatically; verify and commit.

### 7.6 AUR package

Two packages: `bookforge` (builds from source) and `bookforge-bin`
(uses release binary).

`bookforge-bin` PKGBUILD:

```bash
pkgname=bookforge-bin
pkgver=1.4.0
pkgrel=1
pkgdesc="EPUB translation engine that keeps the LLM away from document structure"
arch=('x86_64' 'aarch64')
url="https://github.com/JunjoSick/bookforge"
license=('MIT')
provides=('bookforge')
conflicts=('bookforge')
source_x86_64=("$pkgname-$pkgver.tar.xz::https://github.com/JunjoSick/bookforge/releases/download/v$pkgver/bookforge-x86_64-unknown-linux-gnu.tar.xz")
source_aarch64=("$pkgname-$pkgver.tar.xz::https://github.com/JunjoSick/bookforge/releases/download/v$pkgver/bookforge-aarch64-unknown-linux-gnu.tar.xz")
sha256sums_x86_64=('...')
sha256sums_aarch64=('...')

package() {
  install -Dm755 "$srcdir/bookforge" "$pkgdir/usr/bin/bookforge"
  install -Dm644 "$srcdir/LICENSE" "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
}
```

### 7.7 MSRV audit

Current Cargo.toml specifies `rust-version = "1.95"`, `edition = "2024"`,
`resolver = "3"`. Audit:

1. List all uses of features that require Rust ≥1.85. If none are
   essential, lower MSRV to 1.83 or earlier.
2. Edition 2024 features actually used: list them.  Replace with
   edition 2021 idioms where the cost is small.
3. Resolver 3 is required only for some workspace dependency-resolution
   features. If not strictly needed, drop to resolver 2.

The goal: reduce MSRV to a version that has been stable for at least
6 months. This widens the installable base meaningfully (Debian stable
ships older Rust, NixOS users on stable channel, etc.).

If the bleeding-edge features *are* paying for themselves (e.g. the
project genuinely uses an edition 2024 feature in a hot path that has no
clean equivalent), keep them and document why. Don't compromise correctness
for compatibility; do compromise tooling-bleed for compatibility.

### 7.8 The writeup

> **Status note (2026-07-03):** the writeup was never published — the
> rest of v1.4 (cargo-dist, distribution) shipped without it. It remains
> the only intentional marketing event in the roadmap and is now
> *stronger* than originally scoped: §9's layout-reconstruction work
> ("maybe a short writeup if it turns out interesting" — it did) and the
> measured whitespace-boundary fix are concrete war stories the original
> outline lacked. Treat as open, unscheduled work.

Title (suggested): **"BookForge: Translating EPUBs Without Letting the LLM Near the Structure"**

Outline (~2000–4000 words):

1. The problem with naive LLM EPUB translation (one paragraph; concrete failure modes).
2. The structure-sacred invariant: program owns EPUB, model only sees JSON prose.
3. Marker-safe and run-preserving translation contracts (with examples).
4. Why we don't need a fill-LLM (explicit contrast with dual-LLM systems).
5. Checkpointing, resume, and the segment cache (the boring reliability layer).
6. The glossary, style, and context architecture (the quality layer).
7. What we explicitly don't do, and why (RocksDB, ratatui, multi-agent QA, LLM repair).
8. Honest closing: built for one reader, shared as MIT.

Post in two or three venues:
- r/rust (focus on Rust patterns and correctness).
- Lobsters (general technical audience).
- Optional: Hacker News (only if the post is *technical* enough; avoid the "Show HN: my project" register).

Avoid posting on r/MachineLearning, r/programming (too broad), Twitter/X
threads, or anywhere that demands a follow-up cadence.

### 7.9 CONTRIBUTING.md

Keep it short and honest:

```markdown
# Contributing to BookForge

BookForge is maintained by one person. Contributions are welcome but
the maintainer's response time is "weeks, not days."

## Issues

- Bug reports: please include the BookForge version, operating system,
  the input EPUB (or a minimal repro), and the command you ran.
- Feature requests: check the roadmap in docs/ROADMAP.md first. If your
  request maps to a milestone, that's the answer to "when." If it doesn't,
  open an issue and the maintainer will discuss.

## Pull requests

- Run `cargo fmt`, `cargo clippy --all-targets --all-features`, and `cargo test`
  before submitting.
- Add tests for any behavior change, especially around EPUB structure or
  the LLM contracts.
- Read the architectural invariants in docs/ROADMAP.md before proposing
  anything that involves the LLM doing structure work.
```

### 7.10 Out of scope (within milestone)

- Comparison matrices vs. other tools. Not honest, hard to keep current,
  and unnecessary if the writeup is good.
- Demo videos. Asciinema is fine if you want; not required.
- A separate marketing site. The README and the writeup are enough.
- Recurring social posting per release. Don't.

### 7.11 Acceptance criteria

1. `cargo dist build` succeeds locally; a GitHub release at the v1.4.0
   tag has prebuilt binaries for all configured targets.
2. `brew tap junjosick/bookforge && brew install bookforge && bookforge --version`
   works end-to-end on a clean macOS machine.
3. `yay -S bookforge-bin && bookforge --version` works on Arch.
4. MSRV audit document committed to `docs/msrv-audit.md` with rationale
   for the chosen rust-version.
5. Writeup published in at least two of the chosen venues.
6. CONTRIBUTING.md exists, is honest, and is referenced from README.

### 7.12 Effort

5–7 days, plus writing time for the post (which can be done in parallel
during the v1.3 → v1.4 development gap). Increased from the original
4–6 days because cargo-dist setup is now part of this milestone.

### 7.13 Dependencies

v1.3 (writeup is more compelling once context and style are real).
v1.1 is no longer a hard dependency for binary distribution since
cargo-dist setup itself moved here.

---

## 8. v1.8 — Structural credibility

### 8.1 Goal

Every translated EPUB validates with EPUBCheck. Every release is regression-
tested against a curated corpus of Standard Ebooks fixtures. Local model
support is documented. Pricing is externalized. The README's final form
honestly states what BookForge does, with the corpus claim as the most
credible single sentence in the document.

### 8.2 Architectural rationale

By v1.4, BookForge has features. v1.5 makes those features *durable*
under change. EPUBCheck-clean output is the load-bearing claim that
separates "experimental tool" from "I trust this with my books." The
Standard Ebooks corpus regression is what keeps that claim true as the
codebase evolves.

### 8.3 Deliverables

- EPUBCheck integration: post-rebuild validation during translate (when
  `--validate-output` is set), and full validation as a standalone
  `bookforge validate` command.
- Standard Ebooks corpus manifest and fetch/test scripts.
- CI integration: small subset on every PR; full corpus nightly/manual.
- Local model presets (Ollama, llama.cpp) via OpenAI-compatible URLs.
- Pricing externalized to JSON.
- README final rewrite with corpus claim and full project story.

### 8.4 EPUBCheck integration

EPUBCheck is a Java tool. BookForge invokes it as a subprocess.

```
scripts/epubcheck.sh:
#!/usr/bin/env bash
# Wraps EPUBCheck. Requires java and epubcheck.jar in PATH or BOOKFORGE_EPUBCHECK env.
set -euo pipefail
EPUBCHECK="${BOOKFORGE_EPUBCHECK:-epubcheck}"
exec "$EPUBCHECK" "$@"
```

CLI integration:

```
bookforge validate book.it.epub --report book.it.report.json [--strict-epubcheck]
bookforge translate ... --validate-output [--strict-epubcheck]
```

`--strict-epubcheck` makes EPUBCheck warnings into errors. Default is
permissive (warnings logged, errors fail).

The validate report includes EPUBCheck output as a structured field:

```json
{
  "schema_version": 2,
  "epub_path": "book.it.epub",
  "epubcheck": {
    "ran": true,
    "version": "5.1.0",
    "status": "valid|warnings|errors|unavailable",
    "messages": [
      {"severity": "warning", "code": "RSC-005", "location": "OEBPS/...", "text": "..."}
    ]
  },
  "bookforge_validators": { ... }
}
```

If `java` or EPUBCheck is not installed: `status = "unavailable"`,
log a warning, do not fail.

### 8.5 Standard Ebooks corpus

`tests/corpus/standard-ebooks/manifest.toml`:

```toml
schema_version = 1
description = """
Curated subset of Standard Ebooks fixtures for BookForge regression testing.
Standard Ebooks (https://standardebooks.org) produces high-quality, public
domain EPUB editions with rich markup that exercises real-world translator
behavior.
"""

[[book]]
id = "stevenson-treasure-island"
title = "Treasure Island"
author = "Robert Louis Stevenson"
url = "https://standardebooks.org/ebooks/robert-louis-stevenson/treasure-island/downloads/robert-louis-stevenson_treasure-island.epub"
sha256 = "PLACEHOLDER-fill-in-on-first-fetch"
size_bytes = 0
features = ["dialogue_heavy", "italics", "verse_excerpts"]
ci_tier = "small"   # small | medium | large

[[book]]
id = "carroll-alice"
title = "Alice's Adventures in Wonderland"
author = "Lewis Carroll"
url = "https://standardebooks.org/ebooks/lewis-carroll/alices-adventures-in-wonderland/downloads/lewis-carroll_alices-adventures-in-wonderland.epub"
sha256 = "PLACEHOLDER"
size_bytes = 0
features = ["illustrations", "verse", "dialogue", "drop_caps"]
ci_tier = "small"

# 6–10 more books, covering: footnotes, tables, RTL passages,
# math (where possible), large novels (>500 pages), short stories
```

`scripts/corpus-fetch.sh`:

```bash
#!/usr/bin/env bash
# Fetches all books from the manifest, verifies sha256, populates the
# tests/corpus/standard-ebooks/cache/ directory.
# Idempotent: skips books already present with matching sha256.
set -euo pipefail
# ... implementation ...
```

`scripts/corpus-smoke.sh`:

```bash
#!/usr/bin/env bash
# For each book in manifest at the requested tier:
# 1. Translate with mock provider (mock-prefix-target) to produce a structurally
#    valid output.
# 2. Run EPUBCheck on the output.
# 3. Run bookforge-validators on the output.
# 4. Compare structural metrics (segment count, image count, chapter count)
#    between input and output.
# Fail if any book fails any check.
set -euo pipefail
TIER="${1:-small}"
# ... implementation ...
```

CI integration:

- On every PR: `scripts/corpus-smoke.sh small` (2–3 small books, mock provider).
- Nightly: `scripts/corpus-smoke.sh large` (full corpus, mock provider).
- Manual / pre-release: full corpus with one real provider (e.g.
  openrouter + cheap model), behind a `BOOKFORGE_CORPUS_REAL_PROVIDER`
  env flag.

Do **not** check the corpus EPUBs into git. They're large; the manifest
fetches them on demand.

### 8.6 Local model presets

Two new presets:

```
--provider-preset local-ollama
    # = --provider openai-compatible
    #   --base-url http://localhost:11434/v1
    #   --api-key-env OLLAMA_API_KEY  (often unused but plumbing exists)

--provider-preset local-llamacpp
    # = --provider openai-compatible
    #   --base-url http://localhost:8080/v1
    #   --api-key-env LLAMACPP_API_KEY
```

Doctor sub-checks:

```
bookforge doctor --provider local-ollama --model qwen2.5:14b
bookforge doctor --provider local-llamacpp --model <whatever>
```

Doctor pings `<base-url>/models` (OpenAI-compatible models endpoint) to
verify the daemon is responsive and the requested model is loaded.

Documentation: a new section in README and a dedicated `docs/local-models.md`
with concrete recipes for Ollama and llama-server.

### 8.7 Pricing externalization

`pricing/providers.json`:

```json
{
  "schema_version": 1,
  "updated_at": "2026-05-01",
  "providers": {
    "openrouter": {
      "models": {
        "deepseek/deepseek-v4-flash": {
          "input_per_million_usd": 0.14,
          "output_per_million_usd": 0.28,
          "input_cache_per_million_usd": 0.014
        },
        "google/gemini-2.5-flash-lite": {
          "input_per_million_usd": 0.075,
          "output_per_million_usd": 0.30,
          "input_cache_per_million_usd": null
        }
      }
    },
    "deepseek": {
      "models": {
        "deepseek-v4-flash": {
          "input_per_million_usd": 0.14,
          "output_per_million_usd": 0.28,
          "input_cache_per_million_usd": 0.014
        }
      }
    }
  }
}
```

Code: load from `pricing/providers.json` (bundled in the binary via
`include_str!`); allow override via `--pricing <file>` flag and via
`BOOKFORGE_PRICING_PATH` env var.

CLI (deferred but plumb in v1.5):

```
bookforge estimate ... --pricing /path/to/custom.json
```

Optional follow-on (not required for v1.5 done):
`bookforge pricing update` — fetches OpenRouter's `/api/v1/models`
endpoint, updates the local cache. Stale-cache fallback if offline.

### 8.8 README final rewrite

After all the v1.5 work merges, the README final form:

1. **Opening paragraph** (kept from v1.1, lightly polished).
2. **Why BookForge** — 2 paragraphs explaining the structure-sacred
   invariant in plain language. This is the "why this and not other tools"
   answer.
3. **Status** — what works, with the corpus claim:
   "Tested EPUBCheck-clean against the Standard Ebooks corpus. See
   docs/corpus.md for the test set and methodology."
4. **Install** — `brew install`, `yay -S bookforge-bin`, `cargo install`.
5. **Quick start** — three commands maximum to first translation.
6. **Commands** — the full operational reference (kept from current README).
7. **QA modes / checkpoints / etc.** — kept.
8. **Honest closing** — built for one reader, shared as MIT, here's the
   roadmap, here's how to file issues.

No comparison matrix. No demo video link unless you've already made one
and like it. No badges beyond CI and crates.io.

### 8.9 Out of scope (within milestone)

- Native Anthropic/Gemini providers (OpenRouter routes to both; revisit
  in v2 if quality demands).
- A pricing UI.
- An automated corpus expansion (e.g. "fetch the latest 50 Standard Ebooks
  books"). The manifest is curated.
- Hosted demo (out of scope for one-maintainer project; deferred).

### 8.10 Acceptance criteria

1. `bookforge validate book.epub` produces a JSON report including
   EPUBCheck output (or `status: unavailable` with clear message if
   EPUBCheck isn't installed).
2. `scripts/corpus-smoke.sh small` runs on a clean checkout (after
   `corpus-fetch.sh small`) and passes.
3. CI passes the small-tier corpus smoke on every PR.
4. `bookforge doctor --provider local-ollama --model <a-model>` succeeds
   when Ollama is running with the model loaded.
5. Translating against a local Ollama endpoint produces a valid EPUB
   (use a small chapter for the test).
6. `bookforge estimate book.epub --provider openrouter --model deepseek/deepseek-v4-flash`
   reads pricing from `pricing/providers.json`, not from hard-coded values.
7. `bookforge estimate book.epub --pricing custom.json` overrides the bundled pricing.
8. README is rewritten per §8.8.

### 8.11 Effort

10–14 days. The corpus work is the long pole; budget 4–6 days for it
including the manifest curation, fetch script, and CI plumbing.

### 8.12 Dependencies

v1.4 (distribution). The CI changes need a working release pipeline.

### 8.13 Implementation notes (post-ship, 2026-06-12)

What actually shipped under the v1.5 tag diverged from this spec. Real-book
testing surfaced extraction and scheduling defects that outranked external
validation, so v1.5 became an extraction/scheduling overhaul instead:
depth-anchored block extraction (div/dt/dd/stray text, nested same-name
blocks), HTML entity resolution, code-block passthrough, best-effort sliding
context (strict fence behind `--context-strict`), short per-block markers
(`<m1>`, prompt contract v2, marker schema v3), NCX/OPF/head-title
translation, a shared translate/resume run engine, v1-fast as the default
profile, an identity-roundtrip harness, a text-coverage metric in `inspect`,
and synthetic CI fixtures. The EPUBCheck + Standard Ebooks corpus scope in
§8.4–§8.8 is still wanted and moves to v1.8, after PDF ingestion (§9).

### 8.14 v1.8 implementation notes (2026-06-20)

The deferred structural-credibility scope shipped in v1.8. EPUBCheck is
invoked directly or through a configured JAR and emits a separate schema-v2
validation report so translation QA reports are never overwritten. The pinned
corpus contains nine Standard Ebooks across small, medium, and large tiers;
all nine passed source validation, identity round-trip translation, structural
metric comparison, and EPUBCheck 5.3.0.

Real-book testing also fixed two structural cases outside the curated corpus:
navigation-list labels are now patched inside their links instead of as direct
`li` text, and PDF-converted EPUBs now include the EPUB 3 navigation document
and modified-date metadata required by EPUBCheck.

### 8.15 v1.8.1 patch notes (2026-06-22)

Full-book DeepSeek retries exposed two post-v1.8 validation gaps. Joined
run-preserving batch responses could contain malformed per-block marker
structure, and long source-language blocks could be returned unchanged or
nearly unchanged without failing the normal translation path. v1.8.1 validates
joined runs per block, uses block-local marker requirements, detects copied
source prose before checkpointing, and hardens optional double-check correction
validation. Cached translations are included in double-check audits.

---

## 9. v1.6 — PDF ingestion hardening

This is the next milestone after v1.5. PDF P0/P1 shipped in v1.5;
v1.6 finishes the practical PDF ingestion work with media preservation,
hardening, and degraded-layout fallback.

### 9.1 Goal

Translate the PDFs the owner actually has and cannot translate today:

1. **Scientific papers** — usually two-column, dense with figures,
   tables, equations, and references. Figures and tables must survive
   visually; prose must translate; nothing may silently disappear.
2. **Unorthodox-layout books** — CCRU-style material: scanned or
   converted PDFs with non-standard typography, mixed layouts, decorative
   text. These must degrade *visibly and gracefully*, never silently.

Output is a **translated reflowable EPUB** (readable on the same devices
all other BookForge output targets). Re-laying-out translated text into
the original PDF geometry is explicitly out of scope: it is a research
problem, it cannot be done deterministically, and it violates §1.2.

### 9.2 Architectural rationale

PDF has no DOM. There are no blocks to own — only positioned glyphs. The
invariants survive by splitting the problem in two:

- **Layout extraction** is delegated to proven external tooling
  (poppler's `pdftohtml -xml`, `pdfimages`, `pdftoppm`, `pdftotext`),
  exactly the precedent §8.4 set for EPUBCheck-via-java: external
  binaries are acceptable; embedded runtimes are not. `doctor` learns to
  report their presence and version.
- **Document reconstruction** is deterministic Rust: poppler's XML gives
  per-line text with x/y/width/height/font; BookForge clusters lines
  into columns, columns into reading order, lines into paragraphs,
  font-size outliers into headings — and emits a synthetic EPUB through
  the existing writer machinery.

From that point on, **nothing is new**: the produced EPUB flows through
the same segmentation, markers, cache, checkpoints, validation, QA,
review, and rebuild as any other book. The PDF milestone is an ingestion
front-end, not a parallel pipeline.

The quality gate is the same one that already exists: the `inspect`
text-coverage metric, plus a conversion report comparing reconstructed
text volume against a raw `pdftotext` baseline, per page. Pages that
reconstruct badly are *flagged*, not hidden.

### 9.3 Deliverables

- `bookforge-pdf` crate: poppler XML parsing + layout reconstruction +
  synthetic EPUB assembly. No C dependencies; talks to poppler binaries
  via subprocess only.
- `bookforge convert input.pdf --out book.epub` CLI command with a
  conversion report (text coverage vs `pdftotext` baseline, per-page
  anomalies, image/figure count).
- `doctor` reports poppler tool availability and versions.
- Committed poppler-XML fixtures so all reconstruction logic is
  unit-testable in CI without poppler installed; end-to-end tests gated
  on tool presence (skip with a printed reason, mirroring the EPUBCheck
  pattern).

### 9.4 Phases

- **P0 — plumbing.** Tool discovery (`POPPLER_PATH` env override, PATH
  fallback), `convert` command skeleton, `pdftohtml -xml` invocation and
  XML parse into a page/line IR. Unit-testable from fixtures.
- **P1 — reconstruction.** Line merge → per-page column detection
  (x-gutter clustering) → reading order → paragraph clustering (leading,
  indent, font continuity) → heading heuristic (font-size percentile) →
  block emission → EPUB assembly → conversion report.
- **P2 — figures.** `pdfimages` extraction, placement by page anchor,
  caption detection ("Figure N", "Fig.", "Table N" prefixes near the
  image) feeding `Caption` blocks.
- **P3 — tables and equations.** v1 policy: detected table/equation
  regions are preserved as page-crop raster images (`pdftoppm` crops) —
  reliable and honest for scientific papers; inline math glyph runs
  become protected spans. HTML table reconstruction is a later, separate
  decision.
- **P3.5 — figure/table layout hardening.** Real BERT read-through after
  P2/P3 exposed remaining defects to fix before calling scientific-paper
  PDF ingestion polished:
  1. **Caption-safe figure crops.** A figure raster must not include the
     source caption text when the EPUB also emits a translatable
     `<figcaption>`. Crop boundaries should snap above the detected
     caption baseline, and the report should warn when text overlap
     suggests a duplicated caption inside the image.
  2. **Media-aware paragraph continuation.** Figures/tables/equations
     should act as layout separators without breaking prose continuity.
     Lowercase or suffix continuations after a media block should be
     joined to the preceding paragraph or flagged as orphan continuations.
  3. **Tighter equation/table crop detection.** Do not rasterize ordinary
     prose fragments, model-parameter snippets, or table-adjacent labels
     merely because they contain numbers, parentheses, or `=`.
     Display-equation detection needs stronger math-density and geometry
     tests; inline math should remain protected text where possible.
  4. **Visual regression fixtures.** Add committed BERT-derived
     `pdftohtml` XML/page fixtures for the known hard pages: Figure 1
     caption boundary, Figure 4 multi-panel crop, Figure 5 vector chart,
     and nearby model-parameter/equation false positives. CI should prove
     the generated EPUB has one logical figure per caption, no duplicated
     caption inside crops, no orphan lowercase paragraph starts, and no
     over-broad equation crop count.
- **P4 — degraded-layout fallback.** Pages whose reconstruction
  confidence is low (coverage gap vs `pdftotext`, column chaos) are
  handled per `--low-confidence preserve|linearize`: `preserve` embeds
  the page as a full-page image (CCRU scan posture: keep the artifact,
  skip translation); `linearize` emits best-effort text order and lets
  translation proceed. Either way the report names every affected page.
- **P5 (optional, post-MVP) — pluggable ML backend.** `--pdf-backend
  marker` for layout models (marker/nougat) when installed, emitting
  into the same page/line IR. Never a hard dependency.

### 9.5 CLI surface

```bash
bookforge doctor --pdf
bookforge convert paper.pdf --out paper.epub \
  [--columns auto|1|2] [--low-confidence preserve|linearize] \
  [--report paper.convert.json]
# then the standard flow:
bookforge translate paper.epub --target Italian ...
```

`translate input.pdf` (implicit convert) is deliberately deferred until
the convert step has earned trust on real papers.

### 9.6 Out of scope (within milestone)

- PDF output / translated-PDF re-layout (see §12).
- OCR for image-only scans (`preserve` posture handles them; OCR is a
  separate decision with separate tooling).
- HTML table reconstruction (raster crops first; reassess after real use).
- DRM'd PDFs.

### 9.7 Acceptance criteria

- A real two-column arXiv paper converts to an EPUB with ≥95% text
  coverage against the `pdftotext` baseline, correct reading order on
  manual inspection, and every embedded figure present.
- A CCRU-style PDF converts with every low-confidence page either
  preserved as an image or linearized — and each one named in the report.
- The converted EPUB passes the identity-roundtrip harness (mock
  translate → visible text unchanged).
- All reconstruction logic runs in CI from committed XML fixtures with
  poppler absent.
- `cargo test`, `cargo fmt`, `cargo clippy` clean.

### 9.8 Effort

8–14 days total. P0+P1 are the load-bearing 3–5 days; P2–P4 are
incremental; P5 only if real books demand it.

### 9.9 Dependencies

None on other milestones. Requires poppler binaries on the user's
machine (documented per-OS install one-liners in README).

---

## 9b. v1.7 — Bilingual output

### 9b.1 Goal

Add a `--mode` flag that lets the user produce bilingual EPUBs:
translation appended after the original (block or inline) instead of
replacing it. Default behavior is unchanged (`replace`).

### 9b.2 Architectural rationale

This is the one product idea worth borrowing from oomol-lab/epub-translator,
absorbed cleanly into BookForge's architecture without any compromise.
For language learners, bilingual readers, and anyone who wants a
verification safety net, this is a high-value mode that's surprisingly
cheap to implement on top of v1.x.

This must come *after* v1.5 because EPUBCheck regression-testing is
what catches the failure modes of cleanly inserting sibling blocks
into XHTML while keeping documents valid.

### 9b.3 Deliverables

- `--mode replace|append-text|append-block` flag (default: `replace`).
- Two new **reassembly modes** in `bookforge-epub`: `append-block` and
  `append-text`. (No new LLM contracts; see §9b.7 for why.)
- Stable CSS classes on translation elements: `bookforge-translation`
  and (optionally) `bookforge-source`.
- Default stylesheet, configurable.
- Per-element-type append policy.
- All existing features (glossary, context, style, validation) work
  unchanged in bilingual modes.

### 9b.4 The three modes

#### `replace` (default, current behavior)

Original element is replaced with the translation. Single-language output.

```html
<!-- Source -->
<p>It was a bright cold day in April, and the clocks were striking thirteen.</p>

<!-- After translate --target Italian --mode replace -->
<p>Era un giorno freddo e luminoso d'aprile, e gli orologi battevano le tredici.</p>
```

#### `append-block`

Translation is appended as a sibling block element with a stable class.
Both languages remain visible. Recommended bilingual mode.

```html
<!-- After translate --target Italian --mode append-block -->
<p>It was a bright cold day in April, and the clocks were striking thirteen.</p>
<p class="bookforge-translation" lang="it">Era un giorno freddo e luminoso d'aprile, e gli orologi battevano le tredici.</p>
```

#### `append-text`

Translation is appended as inline text within the same block, separated
by a configurable separator (default: ` / `).

```html
<!-- After translate --target Italian --mode append-text -->
<p>It was a bright cold day in April, and the clocks were striking thirteen. / <span class="bookforge-translation" lang="it">Era un giorno freddo e luminoso d'aprile, e gli orologi battevano le tredici.</span></p>
```

`append-text` is appropriate for short-form content (aphorisms, captions,
poetry where line-by-line bilingual is desired). `append-block` is appropriate
for prose.

### 9b.5 Per-element-type append policy

Not every element should be appended to. Default policy table:

| Element | append-block | append-text |
|---------|--------------|-------------|
| `<p>` | append after | append inline |
| `<blockquote>` | append after | append inline within last `<p>` |
| `<li>` | append after (as nested `<p>`) | append inline |
| `<h1>` – `<h6>` | append after (as `<p>` with same class plus heading-translation class) | append inline |
| `<figcaption>` | append after | append inline |
| `<aside>` | append after | append inline within last `<p>` |
| `<a>` (inline) | not appended | not appended |
| `<code>`, `<pre>` | not appended | not appended |
| `<table>`, `<th>`, `<td>` | append in cell as nested `<p>` | append inline within cell |
| Empty / whitespace-only blocks | skipped | skipped |

This policy is hardcoded for v1.7. Make it configurable via a TOML file
(`--bilingual-policy <file>`) only if real users ask for it.

**Insertion-point ruling (2026-07-03, clarifying an ambiguity).** In
`append-text` mode the translation is inserted as **exactly one span per
block, at the end of the block's inline content** (or, per the table
above, inside the last `<p>` for blockquote/aside). It is NOT inserted
after each individual text run — a paragraph with mid-sentence inline
markup gets one trailing span, never interleaved fragments. The §9b.4
example is normative; the older "after each source text run" phrasing in
§9b.12.3 was a spec bug and has been corrected.

### 9b.6 CSS

Default stylesheet (injected into the EPUB if not already present):

```css
.bookforge-translation {
  color: #555;
  font-style: italic;
  margin-top: 0.2em;
}

.bookforge-translation[lang="ja"],
.bookforge-translation[lang="zh"],
.bookforge-translation[lang="ko"] {
  font-style: normal;  /* italic is awkward for CJK */
}

p.bookforge-translation {
  /* block-level translation paragraphs */
}

span.bookforge-translation {
  /* inline-level translation spans */
}
```

CLI:

```
--bilingual-css <file.css>          # inject custom stylesheet
--bilingual-style minimal|prominent|inline-only
                                    # picks one of three bundled stylesheets
--bilingual-separator " / "         # used in append-text mode (default " / ")
```

### 9b.7 Bilingual reassembly modes

Critically, **bilingual mode is a reassembly concern, not a model-output
concern**. The LLM is *not* asked to emit both source and target. The
program already owns the source; asking the model to echo it back would
violate §1.1 (the structure-sacred invariant: model translates prose only).

The translation contract used in bilingual mode is the existing
`marker-safe` or `run-preserving` contract — same prompt, same payload,
same response shape, same single target string per segment. The model
emits only the target translation. The reassembler in `bookforge-epub`
then inserts the original source (which it already has, in its IR) and
the translated target according to the selected `--mode` value.

So v1.7 introduces:

- **Two new reassembly modes** in `bookforge-epub`: `append-block` and
  `append-text`. (Plus the existing `replace`, kept as default.)
- **No new translation contracts** in `bookforge-llm`. The contracts
  used during translation remain marker-safe / run-preserving exactly
  as in v1.0–v1.6.
- The reassembly modes consume the same translated-segment data structure
  the existing `replace` mode consumes; they differ only in how they
  splice the result into the output XHTML tree.

For `append-block` reassembly: insert the translated target as a sibling
block element with class `bookforge-translation`, immediately after the
original block, with `lang` attribute set to the target language.

For `append-text` reassembly: insert the translated target as an inline
`<span class="bookforge-translation" lang="...">` immediately after the
original text run, separated by the configured separator string.

The model never sees these decisions and never receives bilingual-specific
prompts. Same prompt, same translation payload; the difference lives
entirely in the deterministic reassembly layer.

### 9b.8 Edge cases

- **EPUB metadata**: `<dc:language>` should reflect bilingual content.
  Add the target language as a secondary `<dc:language>` entry.
- **Table of contents**: chapter titles might be bilingual; default
  policy is to translate chapter titles in `replace` mode and to use
  source-only titles in append modes (so the TOC remains readable in
  the original language; the bilingual content is inside the chapter).
- **Cover image**: untouched.
- **Existing `bookforge-translation` or `bookforge-source` classes**:
  if the source EPUB already uses these class names (extremely unlikely
  but possible from a prior BookForge run), append a generation suffix:
  `bookforge-translation-2`. Not a concern for a long while.
- **Right-to-left target languages**: bilingual blocks need
  `dir="rtl"` on the translated element; default policy handles common cases
  (ar, he, fa).
- **Footnotes**: footnote text appears in the target language as an
  appended block, but the *footnote reference* (the superscript link)
  remains in its original position. Two-pass: translate the body, then
  translate the footnote contents at the end of the document.

### 9b.9 Validation in bilingual modes

EPUBCheck must pass in every mode. The most likely failure modes:

- Inserting a `<p>` as a sibling of an element that doesn't allow `<p>`
  siblings (e.g. inside `<h1>`'s parent has constraints). Per-element-type
  policy handles this.
- Duplicate `id` attributes if the LLM somehow returns content with
  IDs (shouldn't happen because the LLM only sees prose). Validators
  catch this.
- Missing `lang` attribute on translation elements (accessibility issue,
  EPUBCheck warns). Always set `lang` on inserted elements.

### 9b.10 CLI examples

```
# Bilingual block (recommended)
bookforge translate origin.epub \
  --target Italian \
  --provider openrouter \
  --model deepseek/deepseek-v4-flash \
  --mode append-block \
  --out origin.bilingual.epub

# Bilingual inline (poetry, short-form)
bookforge translate poems.epub \
  --target Italian \
  --mode append-text \
  --bilingual-separator " — " \
  --out poems.bilingual.epub

# Custom stylesheet
bookforge translate origin.epub \
  --target Italian \
  --mode append-block \
  --bilingual-css ~/styles/bilingual.css \
  --out origin.bilingual.epub
```

### 9b.11 Out of scope (within milestone)

- A side-by-side two-column rendering inside the EPUB (this requires
  CSS that few readers support; append-block looks the same on every reader).
- Per-paragraph highlighting (clicking source highlights target).
  Out of scope; the review UI is the editorial tool, not the EPUB itself.
- Trilingual or N-lingual modes.
- Auto-detection of "should I use append-block or append-text" based
  on content. The user picks.

### 9b.12 Acceptance criteria

1. `bookforge translate book.epub --target Italian --mode append-block`
   produces an EPUB where each source paragraph is followed by an
   Italian-translated `<p class="bookforge-translation" lang="it">...</p>` sibling.
2. The output passes EPUBCheck.
3. `bookforge translate book.epub --target Italian --mode append-text`
   produces inline bilingual content with exactly one
   `<span class="bookforge-translation" lang="it">...</span>` appended at
   the end of each block's inline content (see the insertion-point ruling
   in §9b.5; the §9b.4 example is normative).
4. `--mode replace` produces output identical to v1.5 (no behavior regression).
5. Glossary, context, and style sheets all apply in bilingual modes.
6. Token usage in bilingual modes is roughly equal to that in replace
   mode (the LLM still translates each segment once; the difference is
   in reassembly).
7. Opening the bilingual EPUB in Apple Books, Calibre, and a standard
   ePub.js viewer shows both languages legibly.

### 9b.13 Effort

5–8 days. Most of the work is the per-element-type policy and the
CSS/`lang`-attribute correctness; the contracts themselves are straightforward.

### 9b.14 Dependencies

v1.6 is scheduled first because PDF translation is the owner priority.
Bilingual output has no hard dependency on PDF internals, but it should
wait until the PDF ingestion surface is useful on real documents.

---

## 9c. Follow-up (proposed 2026-07-03) — source-EPUB reflow

**Not yet scheduled; flagged during v1.6 validation.** The owner's
library contains many EPUBs produced by *third-party* PDF conversions
(Calibre et al.) where every printed line is its own `<p>`. BookForge
correctly preserves that structure, so translations inherit
English-line-length paragraph breaks and read ragged in the target
language (observed on the CCRU Abstract Culture book, 2026-07-03). This
is distinct from §9: it is a *source-quality repair* concern for EPUBs
BookForge did not create.

**Spec (promoted from sketch 2026-07-04, scheduled).**

### 9c.1 Surface

A standalone command only: `bookforge reflow <input.epub> --output
<out.epub> [--report <path.json>] [--dry-run]`. No `--reflow` flag on
`translate` in this milestone — the output is a repaired *source* EPUB
the owner inspects before spending tokens, and keeping the translation
pipeline untouched means cache keys need no thought (the repaired file
simply has different source hashes). `--dry-run` writes the report and
prints the summary without producing an output EPUB. Default report
path: next to the output with `.reflow-report.json` suffix.

### 9c.2 Merge rule

Consecutive sibling blocks A, B merge when ALL of:

1. Both are `<p>` elements. Only `<p>` — headings, `li`, `blockquote`
   etc. never participate in this milestone.
2. A's text ends **without** terminal punctuation. Terminal set:
   `.` `!` `?` `:` `;` `…`, optionally followed by closing quotes/
   brackets (`"` `”` `’` `»` `)` `]`), which still count as terminal.
3. B's text starts with a Unicode lowercase letter.
4. `class` attributes are equal (or both absent); B carries no `id`
   (it may be a link target).
5. Neither block is empty, contains nested block-level elements, or
   contains images/replaced content.
6. Nothing but whitespace sits between them in the parent (no
   intervening elements, comments, or PIs).
7. A contains at least one alphabetic character (ruling 2026-07-05:
   bare page/footnote-number paragraphs from PDF conversions — e.g.
   `<p>1</p>` mid-sentence — must never be glued into prose; observed
   corrupting 3 merges on Acid Communism).

Join with a single space; **dehyphenation**: if A ends in a hyphen
attached to a word character, drop the hyphen and join with no space.
Merges chain (A+B+C…) as long as each successive pair qualifies.
B's inline children are appended to A's children verbatim; A's
attributes win for the merged block.

**`--aggressive` (ruling 2026-07-05, owner-approved; amended same day).**
An opt-in flag that relaxes two conditions:

- Condition 3: B may also start with an uppercase letter, an opening
  quote/bracket (`“` `‘` `"` `'` `«` `(` `[`), or a dash (`—` `–`).
- Condition 4 (amended ruling): class equality is **not** required.
  Calibre-style PDF conversions assign per-line classes (`calibre1`,
  `calibre6`, …) as typographic artifacts of line position, so the
  class guard blocks exactly the continuation lines aggressive mode
  exists for. A's class wins (A's attributes already win); when the
  two classes differ the merge record carries `left_class`/
  `right_class` for audit.

All other conditions hold, plus one extra guard that replaces the
letterless screen condition 3 gave for free: **B must contain at least
one alphabetic character** (mirror of condition 7).
Rationale: the conservative rule leaves continuation lines split when
they begin with a proper noun or bracketed insert (observed on CCRU:
"…review of David" / "Toop's Rap Attack…"); the cost is a real
false-positive risk on verse and deliberately short lines, which is why
it is a separate opt-in level, never the default. Every merge that only
qualified under the relaxed rule is marked `"aggressive": true` in the
report so audits can focus on them.

### 9c.3 Report

JSON report: totals plus one record per merge (resource name, block
index, first ~40 chars of each side) so every structural change is
auditable. Human summary printed at the end (files touched, merge
count, paragraph count before/after).

### 9c.4 Acceptance

1. CCRU Abstract Culture (the motivating book): paragraph count drops
   substantially; spot-checked chapters read as prose; EPUBCheck passes
   on the output. (Ruling 2026-07-05: when the *input* already fails
   EPUBCheck — the CCRU source ships 10 pre-existing errors — the
   criterion is **error parity**: reflow must introduce no new
   messages.)
2. A conventionally-typeset EPUB (e.g. the Fisher sources in `test/`)
   produces **zero or near-zero** merges — the guards must not fire on
   healthy books.
3. `--dry-run` writes no EPUB; the report matches what a real run does.
4. Un-mockable criterion (§15.6): reflow the real CCRU EPUB and read a
   chapter of the output.

Because it deliberately changes structure, it stays opt-in forever and
never runs as part of default translation. Effort: 2–4 days. Priority:
high relative to remaining unscheduled work — it addresses the most
visible reader-facing defect in the owner's actual library.

---

## 10. v2 — open-ended (sketched, not committed)

> **Status note (2026-07-21):** "v2" shipped with a scope chosen at the time —
> the monitoring UI plus web dashboard. The four mini-specs in §10.1.1–10.1.4
> subsequently shipped in v2.3.0 and v2.4.0 and remain below as historical
> design records. Bullets marked shipped are complete; the unmarked bullets
> remain open-ended candidates, not commitments.

This section now serves as both a historical design record and an open idea
list. Shipped labels are definitive; every unmarked entry remains a **sketch,
not a commitment**.

### 10.1 Sketched candidates

- **Semantic equivalence scoring** via in-process multilingual embeddings
  (LaBSE or similar via `candle` or `fastembed-rs`). Soft-warning signal
  for meaning drift. Slots into the existing tiered QA.
- **Fuzzy translation memory.** Keyed on n-gram or embedding similarity,
  not exact hash. Useful for series with repeated phrases.
- **`bookforge-engine` library extraction** — a stable, semver'd public
  API for embedding the engine in other Rust applications. The C ABI
  shim is even later.
- **Broader engine API surface** — HTTP/JSON-RPC beyond the local dashboard,
  with an auth model suitable for non-local clients.
- **Native Anthropic and Gemini translation providers.** Only if quality
  measurements prove the OpenRouter detour is hurting.
- **Streaming translation output** for live token meters during long jobs.
- **Format-adjacent sibling tools**: `bookforge-docx2epub` and similar.
  Strictly siblings, not engine surface. PDF graduated out of this bullet:
  it is now mainline ingestion, spec'd as v1.6 in §9 (decision 2026-06).
- **Shipped in v1.2.x — Glossary auto-extraction.** Retained here as
  historical context; see §5b.
- **Shipped in v2.3.0 — Proper Pause + Stop for in-flight translations.**
  Historical mini-spec: see §10.1.1 below.
- **Shipped in v2.4.0 — In-dashboard review & correction loop.** Durable
  corrections, flags, and the editable Review dashboard are retained as the
  historical mini-spec in §10.1.2.
- **Shipped in v2.4.0 — On-the-fly settings reconfiguration.** Revisioned,
  atomic live configuration is retained as the historical mini-spec in §10.1.3.
- **Shipped in v2.4.0 — Truncation handling + fail-fast alert.**
  Escalation-first handling and systemic-truncation alerts are retained as the
  historical mini-spec in §10.1.4.
- **Unified operation/job envelope (`OperationKind::{Translation, Audiobook}`).**
  Audiobook generation shipped in v2.5.0 with its *own* durable
  `AudiobookManifest` checkpoint — per-chunk progress that survives a restart
  — rather than being forced into the translation `SegmentTranslation` job
  model. That standalone manifest is a **deliberate temporary solution and
  works today**. The future refactor: introduce a common operation envelope so
  `RunState`, the JSONL event log, the store schema, the TUI, and the web APIs
  all carry an `OperationKind`, with audio-specific events (plan created, chunk
  started/finished/cached, stitching, final artifact) reusing the translation
  surfaces instead of duplicating them. Deferred until a third operation kind
  or WebUI audio-progress demand justifies the unification; do **not** force
  audio chunks into `SegmentTranslation` in the meantime. (Owner decision
  2026-07-15: keep the temporary manifest for now, unify later.)
- **Generalize the reflow running-header heuristic (consideration).**
  `is_short_running_header` in `crates/bookforge-epub/src/reflow.rs` carries a
  book-specific `!contains("CAPITOLO")` carve-out — Italian for "chapter" — so
  a chapter label that arrives as a `<p>` (PDF-derived) is not stripped as a
  running header. It is inert for non-Italian books and only *protects*
  content, but it is language-specific logic sitting in a general code path.
  Under consideration (owner 2026-07-15, "seems redundant"): either replace it
  with a language-agnostic signal (a general chapter-label/numeral test, or
  gate on the detected source language) or confirm it is redundant — headings
  are normally real `<h*>` elements that never reach this `<p>`-only path — and
  remove it. Low priority; no correctness impact today.

#### 10.1.1 Mini-spec: Pause + Stop (shipped in v2.3.0)

**Shipped in v2.3.0.** The mini-spec below is retained as a historical design
record.

**Goal.** The Progress surfaces (CLI `--ui`, `watch`, and the `serve`
dashboard) gain working Pause / Resume / Stop controls. Today they are
monitor-only; the de-facto pause is "kill the process and `bookforge
resume`", which works (checkpointing guarantees at most the in-flight
segments are re-paid) but is graceless and invisible to the dashboard.

**Design — three small pieces, in dependency order:**

1. **Cooperative pause in the scheduler (`bookforge-llm`).** A shared
   `PauseSignal` (an `Arc<AtomicU8>`-style tri-state run/pause/stop,
   sibling to the existing `CancellationToken` plumbing) checked before
   each new segment dispatch. On *pause*: stop dispatching, let in-flight
   requests complete and checkpoint, then park. On *stop*: same, then
   exit the run loop cleanly. Job status gains `paused` (store enum +
   JSONL event `JobPaused` / `JobResumed` — additive, §1.5-compatible).
2. **Control channel for detached runs.** Runs spawned by `serve` (and
   CLI runs generally) are not children of the controller, so signal the
   engine via a file the run loop polls at segment boundaries:
   `.bookforge/runs/<job-id>/control` containing `pause|resume|stop`.
   Filesystem-based control matches the existing events.jsonl tailing
   architecture, works across unrelated processes, and needs no new
   dependencies (§1.6). Poll cost is one small read per segment boundary.
3. **Surfaces.** CLI: `bookforge pause <job-id>` / `resume` (extend the
   existing command to also clear a paused control file) / `stop <job-id>`.
   Serve: `POST /api/jobs/{id}/pause|resume|stop` writing the same control
   file (CSRF-protected like existing POSTs), and the Progress screen's
   Pause/Stop/Resume buttons wired up. TUI: keybindings on `watch`.

**Acceptance.**
1. Pausing mid-run stops new provider requests within one segment
   boundary; in-flight segments checkpoint; job shows `paused` in
   `status`, `watch`, and the dashboard.
2. Resume (CLI or dashboard) continues without re-translating any
   succeeded segment (cache/checkpoint identical to today's resume).
3. Stop terminates the run cleanly; `bookforge resume` afterwards works
   exactly as it does after a kill today.
4. A pre-feature job (no control file) runs unchanged.
5. Un-mockable criterion (§15.6): pause/resume/stop a real DeepSeek run
   from the dashboard, verify token spend stops while paused.

**Out of scope:** per-segment abort of in-flight requests (let them
finish; they're checkpointed), scheduling/queueing multiple jobs, pause
across machine reboots beyond what the control file naturally provides.

**Effort:** 1–2 days engine + 1 day surfaces. **Priority:** after v1.7
ships; pairs naturally with §9c in a quality-of-life release.

> **Post-ship note.** The controls landed 2026-07-05 alongside §9c reflow
> (+`--aggressive`). The finalize-stage pause path required a fix pass
> (threading the shared
> PauseSignal into QA/double-check, a run-long control-file watcher, and
> resumable stop) caught by a pre-merge review; see
> `docs/codex-fix-pause-review.md`.

#### 10.1.2 Mini-spec: In-dashboard review & correction loop (shipped in v2.4.0)

**Shipped in v2.4.0.** Durable human corrections and flags, the editable
Review dashboard, and the `correct` CLI path completed this loop. The mini-spec
below is retained as a historical design record.

**Goal.** Make the `serve` dashboard's Review screen *editable* so a
non-developer translator can fix a flagged / `needs_review` segment in
place — edit the translation, save it, and rebuild — without touching the
CLI. Today Review is read-only (`GET /api/jobs/{id}/review`); the only
correction path is the CLI `ingest-flags` JSON workflow + `retry`, which the
dashboard's intended audience (translator friends dogfooding real books)
cannot use. Every real-provider run to date has produced `needs_review`
segments — recurring case: quote-heavy blocks the model leaves partly
untranslated — whose remediation is currently developer-only. Closing this
loop is what turns BookForge from "automated translator you monitor" into
"the translator's cockpit."

**Design — reuse existing seams, no new deps (§1.6):**

1. **Store: human-override provenance.** Persist a corrected translation
   with an explicit manual-source marker (provider/model recorded as
   `manual`, a `human_corrected` flag) so re-runs / QA / double-check never
   overwrite a human edit and reports can distinguish it. Additive
   column(s) only (§1.5 events / §11.2 forward migration).
2. **Engine/CLI: a correction is just a translation write + rebuild.**
   Reuse `save_translation` and the existing rebuild path
   (`persist_corrected_translations` / `rebuild_epub_with_options`). Add
   `bookforge correct <job-id> --segment <id> (--text … | --from-file …)`
   as the CLI twin so the loop is scriptable and mock-testable.
3. **Serve: editable Review + save.** New
   `POST /api/jobs/{id}/segments/{segment-id}/translation` (CSRF-protected,
   localhost-only, same pattern as the §10.1.1 pause/stop POSTs). The
   Review screen renders each segment's source + current translation;
   flagged / `needs_review` ones get an inline editable field with **Save**
   and **re-translate with a hint** (feeds `ingest-flags`-style guidance
   into a single-segment retry). Saving persists the override and triggers a
   rebuild; the events stream reflects it.
4. **Idempotency + status.** A manual override marks the segment terminal,
   clears its review flag, and recomputes job status (mirrors the
   finalize-status handling added in §10.1.1). Resume / QA / double-check
   must treat `human_corrected` segments as frozen.

**Acceptance.**
1. In the dashboard, a `needs_review` segment can be edited, saved, and the
   rebuilt EPUB reflects the change — no CLI, no restart.
2. A human-corrected segment is never overwritten by a later resume, QA, or
   double-check pass.
3. The correction is auditable (report shows it as `manual`) and the output
   stays EPUBCheck-clean.
4. CLI `bookforge correct` and the dashboard path produce equivalent results
   (mock-provider e2e; assert on rebuilt-EPUB SHA where applicable).
5. A pre-feature job with no overrides behaves exactly as today.

**Out of scope:** full side-by-side/WYSIWYG diff editor, multi-user editing
or locking, translation-memory suggestions while editing (that's the
fuzzy-TM sketch in §10.1), batch bulk-edit. Keep it one-segment-at-a-time.

**Effort:** ~1 day store+engine+CLI, ~1–2 days dashboard. **Priority:**
high — completes the review loop the v2 dashboard started and is the top
blocker for the non-developer audience.

#### 10.1.3 Mini-spec: On-the-fly settings reconfiguration (shipped in v2.4.0)

> **Shipped in v2.4.0.** Revisioned
> overrides now apply in-process at request, unstarted-batch, and finalize-stage
> boundaries. The CLI and dashboard share validation and persistence; a runtime
> lease lets dashboard Resume signal a live worker or launch one replacement
> after stop/crash. This mini-spec is retained as a historical design record.

**Goal.** Adjust *cache-safe* run settings on an existing (initially: paused)
job and have `resume` apply them — without starting a fresh run and losing
progress. Motivated directly by real use: a run hit repeated output
truncation on an extreme target (Italian → Toki Pona), and the only way to
raise `--batch-max-output-tokens` / lower `--batch-max-items` was to abandon
the job and re-launch from zero. Run settings are baked into the
`RunConfigSnapshot` at launch and replayed verbatim on resume; there is no
seam to amend them.

**Design — CLI-first, reuse existing seams, no new deps (§1.6):**

1. **CLI command.** `bookforge reconfigure <job-id> [flags…]` writing only the
   allowed knobs, e.g. `--batch-max-output-tokens`, `--batch-max-items`,
   `--batch-target-tokens`, `--concurrency`, `--qa`, `--double-check`,
   `--validate-output`, `--provider-max-attempts`, adaptive-concurrency
   toggles.
2. **Persistence: an overrides sidecar.** Write
   `.bookforge/runs/<job-id>/overrides.json` (additive, no migration — matches
   the control-file architecture from §10.1.1). A long-lived runtime watcher
   validates and publishes immutable snapshots to the live scheduler. Resume
   preloads the same durable document before its first dispatch, so stopped and
   crashed workers use identical effective settings without a startup race.
3. **Guardrails — reject cache-affecting settings.** Only settings that change
   *how remaining work is scheduled/budgeted* are mutable. Immutable
   (would make partial output inconsistent or invalidate the cache):
   provider, model, source/target language, profile, context window/scope,
   prompt version, glossary/style/entity inputs. Attempting to change these
   fails with a clear message explaining a fresh run is required and why.
4. **Surfaces.** The CLI, status output, additive progress events, terminal UI,
   and the dashboard Progress screen expose the effective revision. The
   dashboard editor is available for running, paused, and stopped jobs with
   unfinished translation or finalize work.

**Acceptance.**
1. Reconfigure a paused job's `--batch-max-output-tokens` + `--batch-max-items`,
   resume, and the remaining batches use the new budget; already-succeeded
   segments are not re-translated (cache/checkpoint identical).
2. Attempting to change provider/model/language is rejected with a clear,
   actionable message.
3. Overrides are auditable (visible in `status`), additive, and a job with no
   overrides resumes exactly as today.

**Out of scope:** mutating an in-flight provider request or an already-started
finalize stage; changing model/provider or any cache-affecting identity;
reconfiguring completed jobs.

**Effort:** ~1 day CLI + snapshot-merge. **Priority:** high — small, and it
removes a real "lost all my progress to change one number" cliff.

#### 10.1.4 Mini-spec: Truncation handling + fail-fast alert (shipped in v2.4.0)

> **Shipped in v2.4.0.** Truncated batches
> escalate their output budget before splitting, and systemic exhaustion emits
> an additive alert surfaced by CLI, watch, and the browser dashboard. This
> mini-spec is retained as a historical design record.

**Goal.** Handle `max_output_tokens` truncation intelligently, and surface a
prominent, actionable **alert** when truncation is *systemic* instead of
spiraling silently. Surfaced by the Toki Pona limit test: a minimal-vocabulary
target forces long circumlocutions, and its words fragment into many subword
tokens, so output token count vastly exceeds the input-proportional budget
(`input × 3–5`, capped 32k). Every batch truncated mid-JSON; the scheduler's
reflex is to *split* (`batch_invalid_response_split`), but the per-batch budget
is proportional to item count, so each split gets a *smaller* ceiling and
truncates again — recursing to single items that also fail. Result: ~5 minutes
of churn, mostly failures, with no clear signal to the operator.

**Design (in `bookforge-llm` batch scheduler, `batch.rs`):**

1. **Distinguish truncation from invalid JSON.** The failure branch already
   knows `RequestStatus::Truncated` vs invalid JSON — act on it differently.
2. **Escalate-then-split.** On truncation, first **retry the same batch with an
   escalated `max_output_tokens`** (e.g. toward the model cap / a bounded
   multiple of the last budget) before falling back to splitting. Splitting is
   correct for isolating a few pathological items; it is backwards for a budget
   that was simply too small.
3. **Fail-fast alert on systemic truncation.** Track the truncation rate; when
   it crosses a threshold (e.g. N consecutive batches, or >X% still truncating
   after escalation), emit a prominent additive alert event (new
   `ProgressEvent` variant or a distinct `Warning` kind, §1.5) surfaced in CLI
   `--ui`, `watch`, and the dashboard — with an actionable message ("output
   budget repeatedly exhausted; the target/model may be producing runaway
   output — raise `--batch-max-output-tokens`, lower `--batch-max-items`, or try
   a different model"). Optionally auto-park (pause) rather than burn tokens.

**Acceptance.**
1. A single legitimately-long segment that fits under the model cap succeeds
   after escalation instead of being marked failed.
2. On systemic truncation, a clear alert appears within a bounded number of
   failures (not a multi-minute silent spiral), and spend is bounded.
3. Events are additive (§1.5); no new dependencies (§1.6); normal runs (no
   truncation) are unaffected and byte-stable.

**Effort:** ~1 day. **Priority:** medium-high — bounds cost and confusion on
hard targets; pairs naturally with §10.1.3 (the alert tells you what to
reconfigure).

### 10.2 Explicit non-goals (still)

- LLM-driven DOM repair. Architectural invariant violation.
- Dual-LLM "fill" pattern. Architectural smell.
- RocksDB/Sled migration. Premature optimization.
- General-purpose GUI/Tauri/Electron application. The shipped local web
  dashboard is intentionally a localhost operator UI, not a hosted app.
- Multi-agent debate QA. Poor cost/quality profile for translation.
- Hosted SaaS demo. Wrong scope.
- Manga / comic book translation. Different engine entirely.

---

## 11. Cross-cutting concerns

### 11.1 Versioning policy

BookForge follows semver, applied to:

- **CLI**: flags and commands. Once shipped in v1.x, they exist in all
  v1.x. Removed only in v2.0. New flags are minor-version compatible.
- **JSONL event schema**: event types and required fields are stable
  within v1. New event types and new optional fields are minor-version
  compatible.
- **SQLite schema**: forward migrations only within v1. Migrations are
  numbered, idempotent, and always run on startup.
- **Library API** (post-engine-extraction in v2): standard Rust semver.

Bug fixes are back-ported to the previous major for at least 6 months
after a major-version bump.

### 11.2 Schema migrations

```
crates/bookforge-store/migrations/
  0001_initial.sql
  0002_v1_0_1_input_snapshot.sql
  0003_v1_1_segment_flags.sql
  0004_v1_2_glossary.sql
  0005_v1_3_entities.sql
  0006_v1_5_pricing_cache.sql
  0007_v1_7_bilingual_metadata.sql
```

Each migration is a `.sql` file with idempotent `CREATE TABLE IF NOT EXISTS`,
`ALTER TABLE`, etc. The migration runner stores the highest applied
migration number in a `_migrations` table and runs everything above
that number on startup.

Down migrations are not provided. If a user needs to roll back, they
delete `.bookforge/jobs.sqlite` and lose their job history. This is
acceptable because the snapshot files in `.bookforge/runs/<job-id>/`
preserve the inputs and outputs; only the database metadata is lost.

### 11.3 Prompt versioning (major / minor split)

```
prompts/
  segment-translate/
    v1/
      template.txt        # contract version 1
      meta.json           # { "contract_version": 1, "text_revision": 7 }
    v2/
      template.txt        # contract version 2 (breaking change)
      meta.json
```

- **Contract version** changes when the JSON schema of the input/output
  payload changes, when the system prompt fundamentally changes role,
  or when the marker syntax changes. This invalidates the cache.
- **Text revision** changes for prose-level prompt edits: rephrasing,
  tightening, clarifying. Does NOT invalidate the cache.

The cache key is keyed on `contract_version`, not `text_revision`.

When iterating on prompts: bump `text_revision` for prose changes; bump
`contract_version` (and create a new v-folder) for breaking changes.
Old cache entries with old contract versions remain in the DB but are
not reused; they can be GC'd via a future `bookforge gc` command.

### 11.4 Event schema stability

The JSONL event format is documented in `docs/events.md`. Events serialize as
externally tagged `ProgressEvent` variants such as `JobCreated`,
`RequestFinished`, `SegmentFinished`, and `TranslationFinished`.

Within v1.x, event types are not removed. New event types are additive.
New fields on existing events are additive and optional.

### 11.5 Testing strategy

- **Unit tests**: per-crate, alongside source files. Run via `cargo test`.
- **Integration tests**: `tests/` at workspace root. Use the mock
  provider exclusively. Cover end-to-end flows (translate → review →
  ingest-flags → retry) that span multiple crates.
- **Mock provider determinism**: the mock provider must produce
  identical output for identical input. Useful for snapshot testing
  EPUB rebuild output (assert on the SHA256 of the rebuilt EPUB).
- **Corpus regression** (v1.8+): `scripts/corpus-smoke.sh small`
  on every PR; full corpus nightly.
- **EPUBCheck verification** (v1.8+): every translated output in
  corpus tests must be EPUBCheck-clean (or, in `--strict-epubcheck`-
  off mode, free of errors with warnings logged).
- **Real-provider smoke**: manual, before each tagged release. Translate
  one chapter of one corpus book with one real provider. Eyeball the output.

### 11.6 Adaptive concurrency: header awareness

Already partially present. Remaining work, scattered across milestones:

- Parse `Retry-After` header on 429 responses; back off accordingly.
- Parse `X-RateLimit-Remaining` and `X-RateLimit-Reset` where present;
  use to adjust steady-state concurrency.
- OpenRouter-specific limits: parse their custom headers if exposed.
- Document the adaptive behavior in `docs/concurrency.md`.

This is technical-debt-tier work; do it whenever you're in the
relevant code in `bookforge-llm`. Not a milestone gate.

### 11.7 Streaming long segments

Deferred to v2. The motivation is UX: a 200K-token novel translation
sometimes spends minutes on a single segment, and the user has no
in-progress signal. Streaming the partial response to a per-segment
progress update would improve perceived throughput.

Implementation note: streaming complicates response validation (you
have to validate the JSON structure once the stream completes).
Don't attempt unless there's a real demand signal.

---

## 12. Things explicitly NOT in scope (full list)

These are listed here because they will come up — in feature requests,
in your own brain at midnight, in roadmap-drift moments. Each has been
considered and rejected.

| Feature | Why not |
|---------|---------|
| LLM-driven DOM repair | Violates §1.1 architectural invariant |
| Dual-LLM "fill" pattern | Architectural smell; deterministic fill is better |
| RocksDB / Sled storage backend | Premature optimization; SQLite handles this load trivially |
| General-purpose GUI / Tauri / Electron | Wrong scope; the shipped web dashboard is a local operator UI |
| Multi-agent debate QA | Poor cost/quality profile for translation |
| Hosted demo / SaaS | Wrong scope for one-maintainer project |
| Native Anthropic/Gemini providers (pre-v2) | OpenRouter routes to both; prove necessity first |
| DOCX/MOBI/AZW3 input | Sibling tools, not engine surface. (PDF input was on this list until 2026-06; owner priority moved it mainline as v1.6, §9. PDF *output* — re-laying-out translated text into the original PDF geometry — remains out of scope.) |
| Manga / comic translation | Different engine entirely |
| Trilingual+ output | YAGNI |
| Comparison matrices in README | Hard to keep current, not honest |
| Demo videos as required marketing | Optional; technical writeups outperform for the audience that matters |
| CONTRIBUTING.md before contributors exist | Done in v1.4, kept minimal and honest |
| Recurring per-release social posting | Trap; one writeup per major milestone, max |
| Stars-driven roadmap changes | The audience is one reader; the FOSS contribution is the side effect |

---

## 13. Marketing and release posture

Six intentional moments across the v1.x cycle:

1. **v1.1 — Honest README opening**. Not marketing; truth. One paragraph.
2. **v1.1 — crates.io publish + GitHub topics**. Passive discoverability.
   Costs hours, pays for years.
3. **v1.4 — One technical writeup, two or three venues**. Not a launch;
   a record of architecture. Optimize for the comments from the few
   people who know more than you, not for upvotes.
4. **v1.8 — README final rewrite citing the corpus**. The single most
   credible sentence ("Tested EPUBCheck-clean against the Standard Ebooks
   corpus") replaces a thousand comparison matrices.
5. **v1.6 — PDF release notes**. Maybe a short technical note if the
   layout reconstruction and media preservation are interesting enough.
6. **v1.7 — Release notes only**. No promotion. Bilingual mode is a
   feature for those who already use the tool.

Total marketing surface: roughly 5% of project effort, episodic, never
recurring. Anything beyond this is theatrical and warps the project.

---

## 14. Glossary of terms used in this document

- **Architectural invariant**: a constraint that cannot be relaxed without
  fundamentally changing the project's identity. See §1.
- **Block-level vs. inline-level**: in HTML/EPUB terminology, block-level
  elements (`<p>`, `<div>`, `<blockquote>`) take their own line; inline
  elements (`<span>`, `<a>`, `<em>`) flow within text.
- **Contract version (prompt)**: the major component of a prompt's
  versioning. Cache invalidates on contract version change.
- **EPUBCheck**: the canonical EPUB validation tool, maintained by W3C.
  Java-based; invoked as a subprocess.
- **Marker-safe contract**: a translation contract where the LLM is asked
  to translate prose containing opaque markers (e.g. `[[M1]]`) and to
  preserve them in the output. Used to lossly-transmit inline formatting.
- **Run-preserving contract**: a translation contract where the LLM
  receives a sequence of "runs" (text fragments with associated styling
  metadata) and translates each, preserving the run boundaries. Used
  for finer-grained inline formatting preservation.
- **Scope (glossary/style)**: one of `global`, `series`, or `book`.
  Determines which entries apply to a given translation job. Entries
  merge with book-scope winning over series-scope winning over global-scope.
- **Segment**: a unit of translation work. Roughly corresponds to a
  paragraph or a small group of paragraphs. The IR's atomic unit.
- **Soft warning**: a non-fatal flag emitted by the validators during
  translation. Logged in the QA report; surfaced in the review UI;
  never fails a job.
- **Sliding context**: previous N source-target pairs injected into
  the next segment's prompt. Token-budgeted, scope-restricted, and
  filtered to exclude failed/needs-review pairs.
- **Standard Ebooks**: a project producing high-quality, public-domain
  EPUB editions (https://standardebooks.org). Used as a regression
  corpus for BookForge's structural correctness.
- **Status (segment)**: one of `queued`, `in_progress`, `completed`,
  `failed`, `needs_review`. Drives retry logic and review UI filters.
- **Status (glossary entry)**: one of `user_seeded`, `auto_candidate`,
  `accepted`, `rejected`. Only `user_seeded` and `accepted` are
  injected into prompts.
- **Text revision (prompt)**: the minor component of a prompt's
  versioning. Does not invalidate cache.

---

## 15. Document maintenance

This roadmap is a living document. After each milestone ships:

1. Update §2 (overview table) to mark the milestone as shipped.
2. If the implementation diverged from the spec, update the relevant
   milestone section with a "Implementation notes (post-ship)" subsection
   documenting why.
3. If feedback from the milestone changed the priorities of subsequent
   milestones, update those milestones' specs.
4. After v1.4 ships and the writeup is published, recompute v2 from
   the feedback received. Replace §10 with whatever the new v2 actually is.

5. Decisions made in working documents (e.g. `docs/codex-handoff-*.md`
   task specs, review fix-pass rulings) must be folded back into the
   relevant milestone section here once the work ships. Handoff docs are
   scratch; this document is the record. A ruling that only lives in a
   handoff doc will be lost.
6. Every milestone's acceptance criteria must include at least one
   **un-mockable** criterion — a real end-to-end run on a real input with
   real external tools, verified by inspection. The v1.6 review found
   that every confirmed bug lived beyond the stubbed-tool boundary;
   unit-test-shaped criteria alone would have shipped all of them.

The maintainer is the source of truth; this document is the maintainer's
externalized memory. When in doubt, ask.

---

## 16. v2.6.0 — Audiobook narration quality & UI parity

### 16.1 Goal

Make BookForge narration sound and behave like a finished audiobook. Chapter
titles and in-section headings become distinct narration units with deliberate
pauses; neighboring ElevenLabs requests receive enough context to maintain a
consistent delivery; and a metadata-bearing, chaptered `.m4b` becomes the
default artifact whenever ffmpeg is available. The CLI and browser dashboard
must expose the same planning, provider, output, and cost controls without
requiring users to understand the internal chunk pipeline.

### 16.2 Architectural rationale

The v2.5 pipeline proved the provider abstraction and resumable audio cache,
but flattened document hierarchy into prose and treated book assembly as an
optional post-processing step. Those choices are visible to listeners as
poorly announced chapter transitions, inconsistent joins, and an output layout
that requires extra flags to reach the artifact most audiobook players expect.

Hierarchy remains deterministic BookForge data: the EPUB/PDF readers identify
titles, headings, and paragraphs; providers still receive text rather than
document markup. Context and ElevenLabs-specific break tags are additive
request hints, while pauses are inserted at stitch time so every provider gets
the same structure and existing encoded chunks can still be concatenated with
stream copy. Cost and quota checks happen before synthesis so users can make an
informed decision before spending provider credits.

### 16.3 Deliverables

- A typed narration plan in which chapter titles, in-section headings, and
  body paragraphs remain distinguishable. Title and heading blocks occupy
  their own chunks and are never packed into adjacent prose.
- Provider-independent silence insertion with defaults of 1200 ms between
  chapters, 800 ms after titles/headings, and 0 ms between body chunks, plus
  optional ElevenLabs `<break>` tags on supported models.
- ElevenLabs continuity controls: same-chapter `previous_text` and `next_text`
  context (300 characters on each side), a reproducible seed, supported
  language selection, and text-normalization policy.
- A chaptered `audiobook.m4b`, with chapter markers and title/artist metadata
  (using the EPUB author as the artist), as the default deliverable when ffmpeg
  is present. Users may opt out of the book file or additionally request a flat
  single audio file for on-the-go listening. Optional whole-book loudness
  normalization targets `I=-18:TP=-2:LRA=11`.
- Per-character provider pricing, plan/dry-run cost estimates, and a non-fatal
  ElevenLabs subscription-quota preflight before live synthesis.
- Chapter-range selection and provider-backed ElevenLabs voice listing in the
  CLI.
- Browser-dashboard parity: ElevenLabs `Auto (recommended)` model selection,
  a server-side voice-list proxy (`GET /api/audio/voices`), pre-launch
  cost/quota estimates (`POST /api/audiobook/estimate`), per-chapter progress,
  in-page `.m4b` playback, and an Advanced section for chapter pause,
  flat-file output, loudness normalization, seed, and language.

### 16.4 Schema changes

- Audiobook manifest `schema_version` moves from 2 to 3. Chunk records gain a
  `kind` (`title`, `heading`, or `body`), and the manifest gains `seed`,
  `language`, `text_normalization`, `gaps`, and `author`. Every new field is
  serde-defaulted so v2 manifests remain readable by stitch and serve paths.
- The synthesis cache domain tag changes from `bookforge-audio-v1` to
  `bookforge-audio-v2`. The new hash includes narration kind, neighbor context,
  seed, language, and text normalization, deliberately invalidating every
  existing audio chunk cache. This is a one-time compatibility break for
  users' on-disk audio caches, not a semver-major CLI or API change; rerun with
  `--prune` to remove superseded chunks.
- New `crates/bookforge-cli/pricing/audio-providers.json`, with
  `schema_version: 1`, records per-character TTS pricing by provider and model
  (expressed as cost per million characters and/or credits per character).

### 16.5 CLI surface

- `--gap-chapter-ms <MS>` (default `1200`), `--gap-title-ms <MS>` (default
  `800`), and `--gap-paragraph-ms <MS>` (default `0`) control stitch-time
  silence.
- `--break-tags <auto|off>` (default `auto`) enables heading break tags only
  for compatible ElevenLabs v2 models.
- `--seed <U32>`, `--language <CODE>`, and
  `--text-normalization <auto|on|off>` expose ElevenLabs consistency controls.
  Language defaults from the book metadata and is sent only to Flash/Turbo
  v2.5; BookForge warns and drops it for every other model or provider.
- `--loudnorm` normalizes assembled book files; per-chapter files remain
  untouched to avoid independently normalized chapter-to-chapter level jumps.
- A normal run creates the chaptered `.m4b` when ffmpeg is available.
  `--no-book-file` opts out, while `--single` additionally creates a flat
  single audio file. Existing `--stitch` and `--m4b` remain explicit
  overrides.
- `--chapters <RANGE>` selects one-based chapter numbers and ranges without
  renumbering output files; `--list-voices` lists ElevenLabs voice IDs, names,
  categories, and labels without requiring an input book.

### 16.6 Implementation notes

- This milestone builds on the v2.5.x ElevenLabs hardening: request validation,
  retry behavior, provider-specific format handling, and model-specific
  character limits (40,000 for Flash/Turbo v2.5, 10,000 for Multilingual v2,
  and 5,000 for Eleven v3).
- Omitted ElevenLabs models retain the established live preference chain:
  `eleven_v3` → `eleven_flash_v2_5` → `eleven_turbo_v2_5` →
  `eleven_multilingual_v2`. The dashboard's empty model value invokes that
  resolver instead of maintaining a second selection policy.
- The v2.5.x OCR endpoint work established the loopback/API-key detection and
  server-side proxy foundations reused by audiobook voice and quota requests;
  provider keys never enter browser responses, logs, or URLs.
- Neighbor context is computed from the ordered plan and stops at chapter
  boundaries. Editing chunk N intentionally invalidates the hashes for N and
  its adjacent chunks. `previous_request_ids`/`next_request_ids` are not used,
  because their serialization requirement conflicts with concurrent
  synthesis.
- Silence files are generated to match probed sample rate and channel count.
  A missing or failed ffprobe warns and continues without gaps rather than
  failing an otherwise usable narration run.
- Loudness normalization happens once at whole-book assembly. The chaptered
  and flat artifacts are re-encoded only when normalization is requested;
  ordinary stitching retains stream-copy behavior.

### 16.7 Out of scope (within milestone)

- A per-chapter audio streaming route in the browser dashboard.
- ElevenLabs `previous_request_ids`/`next_request_ids` serialization.
- Unifying audiobook and translation operations under
  `OperationKind::{Translation,Audiobook}`; the §10.1 owner decision of
  2026-07-15 stands.
- HTTP Range support on the audiobook artifact route. Inline playback ships
  now; efficient seeking remains follow-up work.

### 16.8 Acceptance criteria

1. A fixture containing a chapter title, an in-section heading, and body prose
   produces separate typed title/heading chunks, narrates the title exactly
   once, and never merges either heading with body text.
2. Deterministic tests prove the default title and chapter gaps appear in the
   correct concat order, paragraph gaps remain disabled by default, and m4b
   chapter-marker durations include inter-chapter silence.
3. ElevenLabs request-contract tests show same-chapter context, seed, language,
   and text-normalization fields when configured, omit them at defaults, and
   never carry context across a chapter boundary.
4. Changing any newly hashed synthesis input changes the
   `bookforge-audio-v2` hash; a v2 manifest remains readable, while every
   v1-tagged cached chunk is treated as stale.
5. With ffmpeg available, an ordinary run emits a playable chaptered `.m4b`
   with title/artist metadata and chapter markers; `--no-book-file` suppresses
   it, `--single` adds the flat artifact, and `--loudnorm` applies only during
   whole-book assembly.
6. Plan and dry-run output report character count and estimated dollars and/or
   credits from schema-1 pricing. An ElevenLabs run reports remaining quota,
   warns when the plan exceeds it, and treats a failed quota preflight as
   non-fatal.
7. Chapter-range parsing accepts one-based comma-separated numbers and ranges,
   rejects zero/reversed/invalid input, and filters plan, manifest, and output
   consistently without changing original chapter numbers. Voice listing
   works without an input EPUB and never exposes an API key.
8. The dashboard's auto-model path omits an explicit model, the voice picker
   degrades to free text if its proxy fails, the estimate endpoint is
   CSRF-protected, progress is grouped per chapter, and the finished view
   embeds the assembled `.m4b` for inline playback.
9. **Un-mockable end-to-end:** narrate a real EPUB chapter with live
   ElevenLabs Flash v2.5, verify audible title/heading and chapter gaps, inspect
   the generated `.m4b` metadata and chapter markers, then play that artifact
   successfully from the browser dashboard.
10. The offline workspace test suite passes on Windows, and live checkpoints
    stay within the explicitly approved ElevenLabs credit budget.

### 16.9 Effort

8–12 focused person-days, including offline coverage, three small live
ElevenLabs checkpoints, and dashboard verification.

### 16.10 Dependencies

- The v2.5 audiobook pipeline, provider hardening, per-model ElevenLabs limits,
  and automatic model resolver.
- The v2.5.x local dashboard, server-side credential handling, and OCR endpoint
  foundations.
- ffmpeg for assembled book files and ffprobe for exact gaps and chapter-marker
  timing; synthesis remains useful when those external tools are absent.

> **Shipped 2026-07-21 as v2.6.0.** All ten acceptance criteria were met. The
> un-mockable release gate narrated a real EPUB chapter live with ElevenLabs
> `eleven_v3` rather than the Flash v2.5 model named in criterion 9; the
> resulting audiobook artifacts and dashboard flow were inspected end to end.

---

## 17. Proposed post-2.6 / v3 candidate arc (not committed)

> **Proposal only.** The candidate milestones below are a menu for maintainer
> selection, not a release promise, sequence commitment, or assertion that a
> semver-major release is required. Their effort ranges are deliberately rough.
> Selecting one candidate does not commit the others.

### 17.1 Candidate A — Engine extraction, public API, and unified operations

#### Goal

Extract a stable `bookforge-engine` library that can run BookForge operations
without going through the CLI, then use that boundary to unify translation and
audiobook lifecycle plumbing and, if there is demonstrated demand, expose a
broader authenticated engine API.

#### Architectural rationale

The CLI currently owns orchestration that embedders would need to duplicate.
The library boundary must come first: a remote API should be an adapter over a
coherent engine, not a second orchestration implementation. The current
`AudiobookManifest` remains a valid temporary checkpoint; unification should
replace duplication only after `OperationKind::{Translation, Audiobook}` has a
clear shared lifecycle.

#### Deliverables

- A documented, semver-governed `bookforge-engine` crate with typed requests,
  progress subscriptions, cancellation/control, resume, and artifact results.
- CLI translation and audiobook paths implemented as clients of the same
  engine API, without changing their user-visible behavior by default.
- A common operation envelope covering translation and audiobook identity,
  state, events, checkpoints, and artifacts while retaining migration support
  for existing translation jobs and audiobook manifests.
- A broader HTTP or JSON-RPC surface only after its non-local use case and auth
  model are approved; the local dashboard remains a client, not a parallel
  engine.

#### Schema changes

- Versioned operation records gain an `OperationKind` discriminator and typed
  operation-specific payloads.
- Store, event-log, and manifest migrations preserve readability and resume for
  existing translation jobs and schema-v3 audiobook manifests.
- Any public wire format receives an explicit schema version before non-local
  clients are supported.

#### CLI surface

- Existing commands remain the compatibility surface and delegate to the
  library.
- A broader server mode may gain explicit bind, authentication, and transport
  settings; exact flags are deferred until the API and threat model are chosen.

#### Out of scope

- A C ABI, hosted service, multi-tenant control plane, or general-purpose GUI.
- Forcing audio chunks into `SegmentTranslation` before a compatible common
  envelope exists.

#### Acceptance criteria

1. A small external Rust program can submit, observe, pause/resume, and collect
   artifacts for both a translation and an audiobook through the public crate.
2. CLI and library runs with equivalent inputs produce equivalent manifests,
   events, and artifacts.
3. Pre-extraction translation jobs and audiobook manifests remain readable and
   resumable after migration.
4. If a non-local API ships, authentication is mandatory outside loopback and
   credentials never appear in URLs, logs, events, or browser responses.
5. **Un-mockable:** run a real translation and a real audiobook through an
   external program using the published engine API and inspect both outputs.

#### Effort

Roughly 15–25 focused person-days, with the remote API portion omitted unless
its demand and security model are approved.

#### Dependencies

- The shipped translation scheduler, control plane, event log, local dashboard,
  and audiobook manifest.
- The broader engine API and operation unification depend on completing the
  `bookforge-engine` extraction first.

### 17.2 Candidate B — Translation QA depth and live progress

#### Goal

Improve translation confidence and responsiveness with semantic-equivalence
soft warnings, fuzzy translation-memory suggestions, and streaming progress
that can drive live token meters during long provider requests.

#### Architectural rationale

These are complementary feedback layers, not replacements for deterministic
validation. Semantic scoring can identify meaning drift that structural checks
cannot see; fuzzy TM can reuse related human-reviewed phrasing without
pretending it is an exact cache hit; streaming can report useful progress while
BookForge still waits for a complete validated JSON response before committing
a translation.

#### Deliverables

- A locally runnable multilingual semantic scorer with versioned model and
  threshold metadata, surfaced only as QA evidence and soft warnings.
- A fuzzy TM index with scoped candidates, similarity scores, provenance, and
  explicit acceptance rules separate from the exact content-addressed cache.
- Provider streaming support sufficient for additive progress events and live
  token meters, while buffering the final structured response for ordinary
  validation and deterministic reassembly.
- Review, report, CLI, TUI, and dashboard presentation for scores, TM matches,
  and streaming progress.

#### Schema changes

- QA records gain optional semantic model/version, score, and threshold fields.
- TM records gain source fingerprint, scope, similarity method/version,
  provenance, and accepted/rejected state.
- Events gain additive streaming-progress counters; partial model output is not
  stored as a completed segment.

#### CLI surface

- Opt-in controls for semantic QA, fuzzy-TM lookup, thresholds, and live token
  meters; exact names and defaults remain design decisions for the milestone.
- Status and review commands distinguish exact-cache reuse, fuzzy suggestions,
  and human-approved TM reuse.

#### Out of scope

- Treating embedding similarity as proof of correctness, silently accepting a
  fuzzy match, or replacing human review.
- Multi-agent debate QA or validation/repair of model-produced document markup.

#### Acceptance criteria

1. A pinned multilingual fixture corpus produces reproducible semantic scores
   and catches seeded meaning reversals without failing structurally valid work.
2. Fuzzy TM returns scoped, ranked candidates with provenance and never reports
   them as exact cache hits.
3. Rejecting or accepting a TM suggestion is durable and auditable.
4. Streaming updates drive live meters, but truncated or invalid final JSON is
   rejected under the same validation rules as a non-streaming response.
5. **Un-mockable:** translate a real chapter through a streaming provider,
   inspect live progress, semantic warnings, and at least one reviewed fuzzy-TM
   suggestion, then verify the rebuilt book.

#### Effort

Roughly 12–22 focused person-days, depending on embedding runtime size and
provider streaming differences.

#### Dependencies

- Existing tiered QA, review/correction workflow, glossary scopes, store, and
  provider abstraction.
- Streaming depends on provider support and must preserve the current complete-
  response JSON validation boundary. Candidate A is useful but not required.

### 17.3 Candidate C — OCR productization

#### Goal

Turn the v2.6.0 `--ocr-endpoint` and openai/unlimited-ocr dialect foundations
into a complete scanned-book workflow with a supported OCR deployment path,
resumable batch conversion, and measurable output quality.

#### Architectural rationale

An endpoint abstraction proves integration but does not make OCR a product.
Scanned books need page discovery, batching, checkpointing, reading-order and
layout reconstruction, and quality gates before their text can safely enter the
existing translation IR. OCR remains a deterministic ingestion concern around
an external or managed recognizer; it must not weaken the rule that translation
models see validated prose payloads rather than raw document structure.

#### Deliverables

- A maintainer-approved bundled and/or managed OCR deployment path, while
  retaining the shipped endpoint dialect for independently hosted services.
- Scan detection, page extraction, bounded concurrent OCR, durable per-page
  checkpoints, retry/resume, and batch conversion for scanned books.
- Deterministic reconstruction into BookForge's existing document/EPUB
  ingestion path, including reading order and retained page/image provenance.
- OCR quality reporting for coverage, confidence where available, suspicious
  pages, language mismatch, and pages requiring human review.
- CLI and dashboard planning, progress, review, and artifact download flows.

#### Schema changes

- A versioned OCR manifest records source identity, backend/dialect, page
  checkpoints, extracted text provenance, quality results, and output artifacts.
- Endpoint secrets remain outside manifests and events; migrations preserve the
  v2.6.0 endpoint configuration path.

#### CLI surface

- A dedicated OCR/conversion workflow, plus an explicit path for feeding its
  validated output into translation; command names and whether this is one or
  two steps remain open design choices.
- Planning flags cover page ranges, backend selection, concurrency, retry, and
  quality thresholds without exposing credentials.

#### Out of scope

- Claiming OCR quality without a representative scanned-book corpus.
- Handwriting recognition, model-driven DOM repair, or reproducing arbitrary
  original PDF geometry as translated PDF output.

#### Acceptance criteria

1. A multi-page scanned-book fixture can be interrupted and resumed without
   repeating successful page OCR.
2. Reading order, page/image provenance, and quality flags survive conversion
   into a valid downstream document artifact.
3. Zip-bomb/resource limits, timeouts, filesystem permissions, and secret
   handling match the hardened v2.6.1 security posture.
4. The dashboard and CLI report the same plan, progress, review pages, and
   final artifacts.
5. **Un-mockable:** convert a real scanned book with the supported OCR backend,
   inspect flagged pages and reading order, then translate a representative
   chapter and validate the resulting EPUB.

#### Effort

Roughly 15–30 focused person-days; backend packaging and corpus diversity are
the largest uncertainties.

#### Dependencies

- The v2.6.0 OCR endpoint/dialect foundations, PDF ingestion, EPUB rebuild, and
  the v2.6.1 resource/security hardening.
- Candidate A is optional; if selected first, OCR should use its operation and
  progress APIs rather than introduce another durable job model.

### 17.4 Candidate D — Selective provider and format expansion

#### Goal

Add native Anthropic and Gemini translation providers only if measurements
justify bypassing OpenRouter, and build `docx2epub` or other format converters
as sibling tools rather than widening the translation engine's core document
surface.

#### Architectural rationale

Provider breadth and format conversion solve different edge integrations but
share one constraint: neither should duplicate engine orchestration or weaken
the EPUB-centered architecture. Native providers require evidence that the
OpenRouter route harms quality, capability, latency, or cost. Format tools
should produce validated inputs for BookForge and remain independently scoped.

#### Deliverables

- Evidence and maintainer approval for each native provider before
  implementation, followed by request/response, usage, retry, error, pricing,
  and streaming parity with existing provider contracts.
- A reusable sibling-tool pattern, with `docx2epub` as the first candidate and
  additional formats considered separately rather than promised as a bundle.
- Structural validation and provenance reports for converted EPUBs before they
  enter translation.

#### Schema changes

- Additive provider identifiers and provider-specific capability metadata;
  secrets remain in the existing credential paths.
- Sibling converters own versioned manifests if they need checkpoints; their
  schemas do not become translation-job fields merely for convenience.

#### CLI surface

- Native providers may become new `--provider` values with capability-aware
  validation.
- Format conversion ships as a sibling command or binary such as
  `bookforge-docx2epub`, with explicit input, output, inspect, and validate
  surfaces rather than implicit translation-engine ingestion.

#### Out of scope

- Adding native providers without a measured benefit, supporting every office
  or proprietary ebook format, or treating DOCX as a new core engine IR.
- Hosted provider brokering or a SaaS conversion service.

#### Acceptance criteria

1. Each native provider has contract, retry, usage, and real-call parity tests
   and demonstrates the measured benefit that justified it.
2. Provider selection never changes cache identity or credential handling
   implicitly.
3. A real DOCX converts to a structurally valid EPUB with an inspectable
   provenance report and can then pass through an ordinary BookForge run.
4. Failure in a sibling converter cannot corrupt an existing BookForge job or
   mutate its source artifact.
5. **Un-mockable:** complete one approved native-provider translation and one
   real DOCX-to-EPUB-to-BookForge flow, inspecting costs and final structure.

#### Effort

Roughly 10–20 focused person-days for two native providers plus the first
sibling converter, after the evidence gates; each additional format is a new
estimate.

#### Dependencies

- Existing provider contracts, pricing/usage reporting, credential handling,
  EPUB inspection, and structural validation.
- Native streaming parity depends on Candidate B if streaming is selected.
  Sibling tools benefit from Candidate A's stable public API but must remain
  outside the engine surface.

---

*End of document.*
