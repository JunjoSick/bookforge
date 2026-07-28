# Why the review loop produces nothing, and what to do instead

Status: **analysis only — nothing decided, nothing implemented**
Date: 2026-07-27
Related: ROADMAP §4 (v1.1 review loop), §5.11 (ingest-flags → glossary),
`docs/validator-tooling.md`

## The problem

The review and correction machinery shipped in v1.1 and was hardened in v2.4.0
with corrections protected from model and cache overwrites. It works. It has
never produced a single correction.

This matters beyond tidiness: that loop is the only mechanism that turns reading
into structured data, so it gates the glossary-seeding path described in
ROADMAP §5.11 and any future finetuned translator. A finetune cannot start from
an empty corpus.

## What the store actually shows

Measured 2026-07-27 against the maintainer's real `.bookforge` store.

| | |
| --- | --- |
| Jobs in the store | 30 (42 run directories) |
| `translations.human_corrected = 1` | **0** |
| Rows in `segment_flags` | **0** |
| `bookforge review` artifacts on disk | **2** — dated 2026-06-22 and 2026-07-16 |
| `flags.json` files anywhere on disk | **0** |

The two review artifacts are a month apart and both dead ends.

**Caveat on what this can and cannot show.** The dashboard review screen reads
live from the store via `generate_review_document` and writes no artifact, so
browser review sessions leave no trace and are invisible here. What is certain
is that no correction and no flag was ever persisted by any path.

## What this rules out

The obvious diagnosis — "the export/download/re-import round trip through
`flags.json` is too awkward" — does not survive the numbers. Only two runs ever
got far enough to face that round trip.

More decisively, **a frictionless path already exists**. The dashboard's
`POST /api/jobs/{id}/segments/{segment_id}/translation` applies a correction
in-place: no file export, no second command, no filesystem juggling. It has
produced nothing either.

So "make the correction UI nicer" is not the fix. Something upstream of the UI
is wrong, and a better button would not have been used.

## The diagnosis: the loop is attached at the wrong point

Consider where each thing physically lives:

- **The deliverable is an EPUB.** It leaves the tool completely.
- **The reader is the maintainer's partner** (ROADMAP §2, §4.2, §5.2), on a
  reading device — not at the terminal that ran the job.
- **The correction surface is loopback-only**, on the maintainer's machine.
- **A flaw is noticed** mid-sentence, mid-chapter, often days from a keyboard.

The loop therefore asks: notice a flaw while reading → remember it → later, at a
different machine → open a QA tool → locate that segment among hundreds → type a
correction. Every arrow loses people, and the first one crosses a **person
boundary**: the human who notices the problem is not the human who can record it.

That is a structural mismatch, not an interface defect. No amount of UI work
fixes a loop whose first step requires one person to transmit a fleeting
observation to another person's terminal.

A second effect compounds it. The review surface is organised around **validator
flags** — but measurement in `docs/validator-tooling.md` showed those flags
running at 75–97% false positive during exactly this period. Someone who did
open the review screen saw a list dominated by segments where nothing was wrong,
and largely disjoint from whatever they had actually noticed while reading. The
tool answered a question nobody asked.

## What is probably right instead

The idea "human feedback improves translation" is sound and is not in question.
The assumption worth discarding is that **the operator will re-derive at a
terminal what the reader noticed on a couch.**

Feedback should be captured *at the moment and place of noticing, by the person
who notices*. Two things already in the codebase make that plausible:

1. **Bilingual output already exists** — `BilingualMode::AppendText` and
   `AppendBlock` in `bookforge-core/src/config.rs`. A bilingual EPUB puts source
   beside target inside the reading artifact, so an error is visible exactly
   where someone is already looking, without any tool.
2. **Every serious reading app exports annotations.** KOReader, Calibre, Apple
   Books and Kobo all export highlights and notes in machine-readable form.

The missing arrow is an ingest path. Something shaped like:

```
bookforge ingest-annotations <job-id> --from <highlights-export>
```

mapping a highlight back to its segment by matching the highlighted target text
against stored translations, then treating the note as a correction or a
glossary candidate. Confirmed by grep on 2026-07-27: **no annotation, highlight,
KOReader, Kobo or Calibre ingest path exists anywhere in the codebase.**

This is a hypothesis with a real weakness worth stating: it assumes the reader
will annotate. Many readers do not, and annotating in a reading app is itself
friction — smaller friction, in the right place, by the right person, but not
zero.

## The experiment to run before building anything

Do not build the ingest path yet. The cheapest useful evidence costs an hour and
no code:

**Read one chapter of an actual translated book and record what you would want
to fix, and how you would want to record it.**

That answers, from the real user rather than from theory:

- How many corrections does a chapter actually generate? If the answer is one or
  two, the whole loop may be over-engineered and a finetune corpus is a long way
  off regardless of mechanism.
- Are they *corrections* (this word is wrong) or *preferences* (this register is
  off)? Those want different mechanisms — the second is a style sheet or
  glossary entry, not a per-segment edit.
- Does noticing even survive to the end of the page? If not, only in-place
  annotation can work.
- Would you rather fix it in the book or note it for later?

Run the same exercise with the actual reader if possible, since they are the
person the loop depends on.

## What would change this conclusion

- If a chapter generates many corrections and they are mostly precise word-level
  fixes, the annotation-ingest direction is strong and worth building.
- If corrections are mostly stylistic preferences, invest in glossary and style
  sheets instead, and drop per-segment correction as a data source.
- If a chapter generates almost nothing, then translation quality is already
  past the point where human correction is the bottleneck, and the finetune idea
  should be re-examined on its own merits rather than waiting on this loop.

That last outcome would be genuinely useful to learn early, because it is the
one that changes the roadmap rather than the code.

## Note on the flags-based design

ROADMAP §5.11 specifies that flags of kind `name` become glossary entries. That
remains a good idea and is independent of the surface. Whatever captures
feedback should still be able to produce glossary candidates — the argument here
is about *where feedback is captured*, not about what is done with it afterwards.
