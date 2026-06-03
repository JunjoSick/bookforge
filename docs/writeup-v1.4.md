# BookForge: Translating EPUBs Without Letting the LLM Near the Structure

*Draft — v1.4 launch writeup. Edit to taste before posting. Target
venues per ROADMAP §7.8: r/rust, Lobsters, optionally HN.*

---

I built BookForge because the obvious way to translate an EPUB with an
LLM is wrong, and the second-most-obvious way is wrong in a more
interesting way. This post is about the third way.

## The naive approach, and what it breaks

The naive approach to LLM EPUB translation is: unzip the book, hand each
XHTML file to a model, ask for the same XHTML back in the target
language, zip it up. People build this every weekend. It seems to work.

What actually happens on a real book:

- The model occasionally drops a `<sup>` footnote reference or moves it
  to the wrong sentence. The footnote anchor in the back matter now
  points at nothing. A reader hits the back of the chapter and the link
  goes nowhere.
- The model "helpfully" normalizes a Unicode em-dash to two hyphens
  somewhere in chapter four. The book's typography quietly drifts.
- The model translates a chapter title in the spine but not the
  identical title in the TOC, because the two requests happened to fall
  on different model invocations and the temperature wasn't quite zero.
  EPUBCheck flags the inconsistency. The book opens in iBooks with a
  blank chapter title.
- Twelve thousand tokens into a long chapter, the model hits its output
  budget mid-paragraph and emits truncated XHTML. The XML parser at the
  other end refuses to reassemble the file. The user is told the book
  failed to translate, but doesn't know which chapter or why.
- The cost is one ill-advised refactor away from being three times what
  the user expected, because nothing caches per-segment and an
  interrupted job restarts from scratch.

You can patch any one of these. You can't patch all of them, because
the failure surface compounds. The root cause is that the model is
being asked to do two jobs at once: translate prose, *and* preserve
arbitrary structured XML. It's bad at the second one in proportion to
how good it has to be at it.

## The structure-sacred invariant

BookForge's central design decision is:

> The program owns EPUB structure. The model only ever sees validated
> JSON prose payloads.

Everything else in the codebase is downstream of that one sentence.

In practice this means:

- An EPUB comes in. The Rust side parses it into an internal IR — a
  tree of blocks, runs, footnote references, links, images, with stable
  IDs assigned to every piece of text the model will ever need to
  translate.
- The IR is segmented into translation units. A segment is a JSON
  payload of prose fragments, each tagged with its stable ID. Inline
  formatting (italics, links, footnote anchors) is represented as
  markers inside the prose strings, with documented escape rules.
- That JSON goes to the model. The model emits JSON of the same shape,
  with the prose translated. The response is validated as JSON,
  parsed, and reassembled back into the IR.
- The reassembled IR is serialized back to XHTML by deterministic code.
  Every footnote anchor, every internal link, every image reference,
  every `xml:lang` attribute is preserved because the program never lost
  track of them.

The model never sees XHTML, never produces XHTML, never produces an
ID, never produces an `href`, never normalizes Unicode, never decides
where a chapter break is. It does one thing — translate prose — and the
program checks that it stayed within those lines.

When the response fails validation, the segment is retried. It is
*not* sent to a "repair" model. The roadmap is explicit about this:

> If the model produces malformed output, the response is rejected and
> the segment is retried — never sent to a "repair" model. If repair is
> genuinely required, that is a bug in `bookforge-core` segmentation or
> `bookforge-epub` rebuild logic, and that is where it must be fixed.

This is a deliberate constraint, not a missing feature. Every "repair
LLM" pass I've seen in this space exists because the primary contract
is too loose. Tightening the primary contract is the right fix; bolting
on a cleanup layer is the wrong one.

## Three translation contracts

There are three contracts in `crates/bookforge-llm/prompts/`, picked per
segment based on what's actually inside:

**Plain** — for segments with no inline formatting beyond paragraph
breaks. The payload is the prose, the response is the translation. The
prompt asks for nothing else.

**Marker-safe** — for segments with inline emphasis, links, footnote
references. The prose carries markers like `<b1>...</b1>` that
correspond to specific IR nodes. The contract demands the same set of
markers in the same order in the output; if any marker is added,
removed, or relocated, the response is rejected. The reassembler then
rebinds the markers to their IR nodes and rebuilds the XHTML.

**Run-preserving** — for segments where multiple inline runs of mixed
formatting interleave (the most pathological case, common in
19th-century novels with frequent emphasis shifts). The contract is
stricter: the model is given an ordered list of runs and asked to
return the translated text aligned to the same run boundaries. The
validator confirms count, order, and presence; the reassembler does
the rest.

Each contract has a single-segment and a batch variant, plus a compact
form for high-throughput runs. The versioning is in the filename:
`translate_marker_safe.v1.md`. When the JSON contract has to change
incompatibly, the file bumps to `v2` and the cache namespace bumps with
it. Prose-level edits to a prompt (the model's instructions, the
examples) bump a minor version that does not invalidate cache.

None of these contracts ask the model to think about EPUB. The model
sees prose and markers; the program does everything else.

## Why there is no fill-LLM

Some tools in this space use a second, low-temperature LLM to "fill in
the XML structure" around the translated prose. The pattern is: model
A translates, model B reassembles, and the user gets a working EPUB.

This works often enough to be tempting and fails often enough to be a
problem. The failure mode that matters most is the one where model B
"helpfully" merges two paragraphs because the prose flowed nicely, or
silently drops a stray `<i>` it thought was a noise tag, or invents an
`id` that doesn't exist anywhere in the book. By the time the user
notices, they're hundreds of segments in and the cache is full of
output they can't trust.

The deeper objection is that the fill-LLM is solving a problem that
doesn't need a model. Reassembling JSON-tagged prose into the original
XHTML tree is pure code — given the IR, you walk the tree, you bind
each translated string to its block, you serialize. There's no
ambiguity. The only reason a model gets involved is that the primary
contract leaked structure into the response, and now somebody has to
guess what the model meant.

BookForge avoids the situation by not letting the primary contract leak
in the first place. Reassembly is deterministic Rust over an IR. No
second model. No "structure-aware" pass. If reassembly produces a
broken EPUB, that's a bug in the deterministic code, and bugs in
deterministic code can be reproduced and fixed.

## The boring reliability layer

The least glamorous part of BookForge is the part the maintainer is
proudest of: it doesn't lose work.

Every job has a SQLite checkpoint store at `.bookforge/jobs.sqlite`.
Every segment translation is persisted as soon as it's validated. If
the process dies, the network drops, the laptop sleeps, or the user
hits Ctrl-C, `bookforge resume <job-id>` picks up where the previous
run left off. The original input EPUB is snapshotted into the job
directory at submit time, so resume works even if the source file has
been moved or deleted between submission and resume.

The segment cache is content-addressable. A cached translation is
reused if and only if all of these match:

- the source segment's SHA-256
- the prompt contract major version
- the provider
- the model
- the source language
- the target language
- the glossary content fingerprint (in v1.2+)
- the glossary render format (json vs prose, in v1.2+)
- the context-window settings (in v1.3+)

If any of these change, the segment re-translates. If none of them
change, an interrupted job resumes from cache and only pays for the
remainder. A run that completes on the second attempt with all the
same settings costs no more than a run that completes on the first
attempt minus the network calls for cached segments.

This is invisible when it works and invaluable when it doesn't. The
maintainer's girlfriend reads books on a phone with intermittent wifi;
the maintainer translates books at a desk with reasonable wifi; the
process between them has to survive a dropped connection and a closed
laptop lid mid-chapter. It does, because the SQLite store doesn't
care about transient failures.

JSONL progress events are emitted for every state transition. The
event schema is frozen for v1; new fields are additive, breaking
changes go in v2. You can pipe them to a file, watch them with `tail`,
or wire them into whatever progress UI you want.

## The quality layer

On top of the reliability layer is the part that decides whether the
translation is *good*.

**Glossary** (v1.2). A TOML file maps source terms to target terms,
scoped to a book, a series, or globally. When a segment contains a
glossed term, the term is injected into the prompt as an active
constraint, ranked by relevance and capped at a configurable token
budget. The review HTML highlights segments where the model emitted a
target term inconsistent with the glossary. The series scope is the
load-bearing feature here: translating book three of a series with the
same glossary as books one and two means character names, place names,
and invented terminology stay consistent across the whole arc.

**Sliding context** (v1.3). The previous N segments' source-and-target
pairs are included as context in the next segment's prompt, capped at
a token budget, scoped to the same chapter. This catches the failure
modes that glossary alone can't: pronoun resolution across paragraphs,
narrative voice consistency, register drift across long chapters.
Failed segments are never injected as context — contaminating the next
prompt with a known-bad translation regresses the next one toward it.

**Style sheets** (v1.3). Per-book or per-series TOML files that encode
register, narration tense, dialogue formality default, loanword policy,
and free-form custom instructions. Merged at translate time with the
same precedence as glossaries (book > series > global) and rendered as
a prompt block.

**Entity sheets** (v1.3). A structured list of named entities with
their grammatical gender in the target language. Italian, French,
Spanish, and German all need this; English doesn't. The model gets a
grammatical-agreement table along with each segment, so "she said,
looking at the Ring" doesn't translate with feminine agreement on the
wrong noun.

These layers compose. A job with a series glossary, a book style
sheet, sliding context, and an entity sheet still goes through the
same JSON contract; the prompt block just has more constraints
injected into it. None of them changes the structure invariant.

## What BookForge deliberately doesn't do

The roadmap doubles as a "no" list. A few of the louder ones:

- **No RocksDB, no embedded V8, no JVM components.** SQLite via
  `rusqlite` (bundled) is the entire storage substrate. The binary is
  single-file. EPUBCheck is allowed as an *external* tool invoked via
  `java`, but BookForge itself runs on a machine with nothing but
  itself installed.
- **No multi-agent QA orchestrator.** There is one optional LLM QA pass
  (`--qa suspicious|all`) that runs *after* deterministic validators
  and explicitly does not replace them. It's not a planning loop, not
  a critique-and-revise loop, not a debate framework. It's one model
  call per segment that says "is this translation suspicious?"
- **No fill-LLM, no structure model, no repair model.** Covered above.
- **No "translate the whole chapter in one shot."** Chapters are
  segmented; segments are bounded; the model never gets a payload large
  enough to truncate mid-output. If truncation does happen on a
  pathological segment, that's a bug in segmentation, not in the LLM
  contract.
- **No web server, no daemon, no cloud component.** Review HTML is a
  static file. Flagged segments are exported as a downloaded JSON file
  and ingested back via a CLI command. Nothing phones home.
- **No ratatui TUI.** Progress is an `indicatif` bar, a JSON stream, or
  silence. A full TUI is a feature for someone who is not me.

Each of these is a thing other tools in this space do, and each was
considered explicitly and declined. Most of them violate the single-
binary distribution goal, the structure-sacred invariant, or both.

## Closing

I built BookForge to translate books for my partner. She reads in
Italian; the books I want to share with her aren't in Italian. Every
existing tool I tried produced something that opened in her e-reader
and then made her stop reading three chapters in because the
typography or the consistency was off enough to be distracting.

The architectural decisions in this post are the ones that fix the
specific failure modes I watched her hit. They are not the only
reasonable choices for an EPUB translator — they are the choices that
make the tool work for one specific reader. The fact that they
generalize is a bonus.

It's MIT-licensed in case any of this is useful to you. The roadmap
through v1.6 is public. Issues and PRs are welcome but the maintainer's
response time is "weeks, not days."

If you read all the way through, thank you. Now go translate a book.

---

*BookForge source: <https://github.com/JunjoSick/bookforge>*
*Roadmap: <https://github.com/JunjoSick/bookforge/blob/main/docs/ROADMAP.md>*
