# Measuring validator quality

BookForge flags a translated segment as `needs_review` when a deterministic,
string-level check fires. Those checks have no model of meaning, so some
proportion of what they flag is wrong — and until 2026-07-26 nothing measured
which proportion.

Four dev-time examples cover validator replay, flag precision, translation
quality, and quality-finding precision. None is wired into `translate`, and none
should be: a model that gated or repaired translations would violate the
architectural invariants in `ROADMAP.md` §1.1–§1.2.

```
cargo run --release --example replay_validation -- --help
cargo run --release --example judge_flags -- --help
cargo run --release --example judge_translation -- --help
cargo run --release --example adjudicate_translation -- --help
```

## `replay_validation` — replay the validator offline, for free

`batch_item_validation_error` is a pure function over (source block, model
output). Everything it needs is already on disk: each job's `input.epub`
snapshot plus the per-block translations the run stored. So validation logic can
be re-run over real books without calling a provider.

```
cargo run --release --example replay_validation -- --target-lang Italian
```

It re-derives blocks from every snapshot, pairs them with stored translations,
re-runs validation, and reports flags by kind — plus a delta against what the
original run recorded in `segments.error`. **Zero API calls, zero cost.**

Notes that matter when reading its output:

- The store is opened through a **throwaway copy**, because `JobStore::open`
  migrates on open. The original is never written. Verify with a hash if you
  are nervous; the numbers below were produced that way.
- Jobs whose run snapshot cannot be resolved are **skipped and counted**, not
  silently defaulted. An earlier version defaulted, and the resulting figures
  were meaningless.
- **Preserved-source pairs are reported separately.** A `needs_review` segment
  stores the *source* text as its translation, so replaying those compares
  source against source and says nothing about the model. In one run that
  artifact was 4,750 of 5,139 raw flags. Always read the
  "excluding preserved-source pairs" figure.
- Replay currently calls only the exact-equality message
  `translation_unchanged`. The 92%-overlap message (`translation retains N% of
  the source-language words`) appears under `other`, although the durable
  finding classifier correctly stores both as `source_copy_unchanged`. Inspect
  the `other` messages before concluding that they came from the target-language
  gate.
- `--emit-pairs <file.jsonl>` writes the flagged pairs for the judge below.

Use it to answer "did this validator change help?" before spending anything.

### Language assumptions in deterministic validation

Audited 2026-07-30 against the corpus below:

- Inline marker identity, multiplicity, shape, nesting, and marker-aware prose
  splitting are structural and language-neutral. Reference text inside a marker
  is a separate data check; decimal reference glyphs from non-Latin scripts are
  recognized as references.
- URL, email, filename, internal-anchor, citation, code, and footnote-reference
  protected spans are exact data-identity checks. Math is also identity-based
  but is reported as a warning.
- Number matching accepts localized comma/point decimals, comma/point/space
  grouping, and reordered numeric date components. A leading numeric value may
  keep its value while its word suffix changes, for example `4th` to `4º` or
  `19-August` to `19 luglio`. The value parser still recognizes ASCII digits;
  changing the digit script itself is not treated as numeric equivalence. On
  the real Italian-target replay, this removed 93 flagged pairs (99 individual
  number complaints): all retained the numeric values while translating ordinal
  suffixes, date words, or prose fused to biographical year ranges.
- The small-date severity rule recognizes month context in English, Italian,
  Spanish, Portuguese, Danish, Norwegian, French, and German, including a
  linking word such as `de`. A missing one- or two-digit number outside known
  critical contexts remains a warning.
- Source-copy validation is intentionally cross-language, but its thresholds
  are language-pair-blind: exact equality always fires, while near-copy requires
  at least 120 source characters, 30 source words, 30 overlapping words, and 92%
  multiset overlap. The corpus has no close-pair translation with which to
  calibrate that threshold. The Italian-to-Italian identity job is not such a
  probe: source and target language are equal and its provider is `mock`, either
  of which disables source-copy validation.
- `target_language_gate` is not a generic language detector. It is the strict
  closed-vocabulary and grammar gate for the built-in Toki Pona style and
  returns immediately for every other target language.
- Source-copy content exceptions are not language-neutral. Reference-section
  titles, `p.` page-note syntax, and explicit “in English” glosses are recognized
  with English phrases. Treat results from differently titled reference
  sections with care.

## The corpus these tools run against

Measured 2026-07-29 against the owner's store, so the next person does not
re-derive it: **30 jobs** (28 English-to-Italian, one Italian-to-Italian mock
identity run, and one English-to-Toki Pona), all 30 snapshots resolvable, 27 with
stored translated blocks, **40,576 replayable pairs** across 8 books. The 29
Italian-target jobs account for 40,303 of those pairs. Four independent
translations of *Calling Bullshit* and five of *If We Burn* exist, which is free
A/B material.

`items with no translation` and `block rows with no item` in the thousands are
expected rather than a defect: several jobs are `needs_review` or `stopped`
with a large unfinished tail. A "job" is not a finished book.

### Resolving snapshots: the trap

`input_snapshot_path` is stored **relative to the directory BookForge was
launched from**, with mixed separators (`.bookforge/runs\job_...\input.epub`).
`replay_validation` resolves it against a root derived from `--db`, so pointing
`--db` at a lone copy of `jobs.sqlite` elsewhere resolves nothing:

```
skipped: snapshot unresolved: 29
replayed                    : 0
```

Copy the whole `.bookforge/` directory, or point `--db` at the real store — it
is copied before opening and the original is never written.

## `judge_flags` — ask a model whether a flag is real

The question is **not** "is this translation good?" It is "is this specific
validator complaint correct about this specific translation?" That is a much
narrower question, and models answer it well.

```
cargo run --release --example judge_flags -- --pairs pairs.jsonl --dry-run
cargo run --release --example judge_flags -- --pairs pairs.jsonl \
    --provider openrouter --model moonshotai/kimi-k3 --limit 100
```

Output is a per-kind true-positive / false-positive rate. That number decides
whether a validator is worth keeping, tightening, or demoting.

Operational lessons, all learned the expensive way:

- **`--dry-run` first, always.** It renders prompts and estimates cost without
  calling anything.
- **`--limit` truncates, it does not sample.** Shuffle your pairs first (with a
  recorded seed) or you will judge only the earliest jobs, which is a biased
  read on exactly the thing you are measuring.
- **Raise `--max-output-tokens`.** The 400 default starves reasoning models:
  Kimi K3 returned empty content on **49 of 150** calls at 400, and **4 of 100**
  at 1200. That is a third of the spend wasted. This is the same failure
  described under "Bound each request while keeping the per-candidate allowance
  honest" below, and it has now been hit by **four** separate call sites.
- **Verdicts are cached on disk** by content hash, so re-runs of already-judged
  units are free. Keep the cache directory between runs.
- **Judges disagree, and the stricter one has been right.** On a shared subset,
  deepseek-v4-pro reported 97.3% false positive against Kimi K3's 82.4%. v4-pro
  was caught contradicting its own rationale and dismissing real defects — a
  source `December 8` rendered as `10 dicembre`, which is silent data
  corruption. Treat a single judge's rate as a bound, not a measurement, and
  prefer the stricter model.

## `judge_translation` — measure whether translations are good

Validator volume and precision do not measure translation quality.
`judge_translation` builds passage-sized units from real stored translations
and asks a judge to enumerate defects in six fixed categories:
`meaning_changed`, `content_dropped`, `content_added`,
`terminology_inconsistent`, `register_shift`, and `target_language_error`.

Start with:

```
cargo run --release --example judge_translation -- \
    --db .bookforge/jobs.sqlite --sample 25 --dry-run
```

The dry run opens only a throwaway store copy, renders every sampled system and
user prompt, records the fixed sampling seed, estimates input tokens, shows the
configured output-token cap, and prices the maximum run from the embedded
catalog. It makes no provider calls and writes no JSONL, summary, or cache
entries.

A real run writes `judge-translation.jsonl` and
`judge-translation.summary.json` by default. Use a previous summary for an A/B
comparison:

```
cargo run --release --example judge_translation -- \
    --job <candidate-job-id> \
    --baseline baseline.summary.json \
    --out candidate.jsonl
```

Important measurement rules:

- A passage is a greedy contiguous run of blocks from one EPUB section. The
  next block starts a new passage when adding it would exceed
  `--passage-chars` (default 1,500), when a section changes, or when an
  unavailable stored block creates a gap. Blocks are never split; a single
  oversized block is one oversized passage. Passages may cross scheduler
  segment boundaries within the same section.
- `needs_review` rows are excluded. The store deliberately preserves source
  text for those rows, so they are not translation observations.
- `--sample` performs a deterministic Fisher-Yates shuffle before taking N.
  `--seed` defaults to a fixed value and is present in both every JSONL record
  and the summary. `--sample 0` explicitly selects everything and warns that
  the spend cap is gone.
- The judge returns enumerable defects, not a score or severity. Every finding
  must contain exact non-empty source and translation quotes. Missing quotes,
  non-verbatim quotes, and self-refuting explanations are dropped and counted
  separately in both passage JSONL and the summary.
- The self-refutation filter is deterministic because all three measured
  attempts to suppress non-issues through prompt wording made the QA reviewer
  noisier. It recognizes explicit English dismissals such as `no error`,
  `no issue`, `is correct`, and `correctly translated`. A finding is retained
  if the explanation contains a contrast marker (`but`, `however`, `although`,
  and similar), a separate defect assertion, or a negated/hypothetical
  correctness claim. Thus `X is correct, but Y is wrong` remains a finding.
  This conservative lexical rule can miss paraphrased or non-English
  self-refutations, and it deliberately keeps some fully dismissive
  explanations that happen to use a contrast word. Those are false negatives;
  the rule is biased against silently dropping a genuine mixed complaint.
- Counts and defects per 1,000 source words are reported for every category.
  The three hard categories and three soft categories are also reported as
  separate groups. There is intentionally no combined headline score.
- Unparseable output is recorded once and excluded from the word/rate
  denominator. It is never sent to a repair model. Request failures are also
  recorded rather than converted into zero-defect passages.
- The default judge output cap is 4,096 tokens. This is intentionally generous:
  reasoning models that exhaust their cap have returned HTTP 200 with empty
  content, and valid findings need room for two quotes apiece.
- Results are cached by a hash of the passage content, provider/model, judge
  settings, and `judge_translation` prompt version. The API credential remains
  an environment-variable name supplied by `--api-key-env`; it is never a CLI
  value or an output field. **`--max-output-tokens` is part of that key**, so
  re-running at a different cap re-pays for the whole sample. That is correct —
  a different cap can produce a different answer — but it makes cap tuning cost
  money, so pick a generous cap first.

### What this measured, 2026-07-29

First real run. Corpus: `If We Burn`, English→Italian, deepseek-v4-flash,
133/133 segments complete — chosen deliberately over the larger Lenin jobs,
which are PDF conversions where ~29% of validator false positives were traced
to OCR damage in the *source*. Judging those measures the scanner.

25 passages, one seed, two judges:

| | judged | source words | hard/1k | soft/1k |
| --- | --- | --- | --- | --- |
| Kimi K3, 4k cap | 15/25 | 2,845 | 4.57 | 4.57 |
| Kimi K3, 16k cap | 23/25 | 4,348 | 5.06 | 3.91 |
| deepseek-v4-pro, 16k cap | 25/25 | 4,803 | 4.16 | 0.83 |

Restricted to the identical 15 passages both judges completed, Kimi and v4-pro
report **4.57 vs 4.92** hard defects per 1,000 words and **4.57 vs 0.70** soft.

**The hard-defect rate is judge-stable; the soft-defect rate is not.** Hard
spans 4.16–5.06 across two judges and two output caps — roughly 20%. Soft spans
0.83–4.57, a 5.5× swing driven almost entirely by `target_language_error`, the
most subjective category. This mirrors the earlier finding that structural,
cross-referenced claims are the productive class and isolated-phrase judgements
are the noise class.

So: **track the hard-defect rate. Report the soft rate, but do not optimise
against it, and never compare a soft rate across judges.** This is also why
there is no combined headline score — one number would blend a stable signal
with an unstable one and move whenever the judge changed.

A representative catch, from Kimi: source `bring the media into your
understanding` rendered as `include la media`, which in Italian means *the
average*. The referent changes completely, and no deterministic check can reach
it.

**Cost is wildly asymmetric.** deepseek-v4-pro spent 113 output tokens per
passage; Kimi K3 spent about 2,000. For the same 25 passages that is **$0.013
against roughly $0.50** — about 40× the coverage per dollar.

> **Superseded later the same day.** Adjudication put v4-pro's own findings at
> roughly **23% precision** (see the calibration section below), so the cheap
> coverage buys mostly noise. Do not choose v4-pro as the judge on the strength
> of this cost comparison alone.

**Kimi K3 starved even at 16,000 output tokens** on 2 of 25 passages, and on 10
of 25 at 4,096. It can burn more than 16k reasoning tokens on a 1,500-character
passage. Budget accordingly, and see the empty-content diagnosis note above.

### The accepted-glossary A/B, 2026-07-29 — no measurable effect

The first question this benchmark was built to answer. Protocol below, The
Cyberiad, EN→IT, deepseek-v4-flash translating, v4-pro judging, fresh scratch
store, both arms translated on the same build. Rates computed on the **94
passages whose block sets are identical in both arms** — word counts matched
exactly at 16,945, so exposure is equal.

| | no glossary | 121 accepted terms | p |
| --- | --- | --- | --- |
| hard defects | 170 (10.03/1k) | 188 (11.09/1k) | 0.37 |
| soft defects | 40 (2.36/1k) | 58 (3.42/1k) | 0.09 |
| meaning_changed | 155 | 181 | 0.17 |
| content_dropped | 15 | 6 | 0.08 |
| terminology_inconsistent | 0 | 3 | 0.25 |

**Nothing reaches significance** (two-sided conditional-binomial test at equal
exposure). The glossary neither helped nor hurt measurably, and notably did not
improve terminology consistency — the one thing it exists to do.

**The experiment is underpowered by roughly 10×.** At ~170 findings per arm the
noise floor is about ±19, so only an effect larger than ~20% would be
detectable. One book cannot answer this question; a serious attempt needs on the
order of ten.

**Report a p-value, not a percentage.** An earlier version of this run was read
as "+20% hard defects from the glossary". That reading was wrong twice over: the
run was confounded by the per-block glossary duplication fixed in #89, *and* the
difference was not significant to begin with.

Two things the failed experiment did buy. It surfaced that duplication bug —
worth a 2.75× cost reduction on every glossary-enabled translation — and it is
the reason the precision calibration below exists.

### Self-refuting findings, measured 2026-07-29

The first glossary A/B run exposed a deterministic failure mode in the absolute
counts. Of 587 findings, 67 dismissed their own complaint:

| arm | raw findings | self-refuting | retained |
| --- | ---: | ---: | ---: |
| baseline | 262 | 25 (9%) | 237 |
| glossary | 325 | 42 (12%) | 283 |

For `target_language_error`, 20 of 43 findings (47%) were self-refuting.
Leaving those rows in made the glossary arm look 116% worse on soft defects;
removing them changed that comparison to +24%. This does not make the remaining
findings true. It only establishes the minimum rule that a finding cannot
simultaneously say there is no defect.

Cached passage outcomes written before the filter are filtered when read, so a
cache replay adopts the new rule without another provider call. Old result
JSONL can also be passed directly to the adjudicator below; it defensively
applies the same rule and reports `dropped_input_self_refuting`.

## `adjudicate_translation` — measure quality-finding precision

This is a separate example rather than a mode inside `judge_translation`.
Generation reads a throwaway job-store copy and measures defect incidence;
adjudication reads only the resulting JSONL and measures whether each claim is
right. Keeping those phases separate makes it impossible for calibration to
rewrite a translation or accidentally reopen the owner's store, and lets one
paid generation be calibrated repeatedly with different adjudicators.

As with `judge_flags`, each request asks one narrow question. The input is one
finding's claimed category, exact source span, exact translation span, and
explanation. Start offline:

```
cargo run --release --example adjudicate_translation -- \
    --results judge-translation.jsonl \
    --provider deepseek --model deepseek-v4-pro \
    --limit 0 --dry-run
```

`--limit` truncates in input order; it does not sample. Use `--limit 0` only
after inspecting the dry-run maximum, or externally shuffle with a recorded
seed if a capped precision sample is required. The paid owner-run command is:

```
cargo run --release --example adjudicate_translation -- \
    --results judge-translation.jsonl \
    --provider deepseek --model deepseek-v4-pro \
    --max-output-tokens 1024 --limit 0 \
    --out translation-adjudication.jsonl \
    --summary translation-adjudication.summary.json
```

The content-addressed cache key includes
`adjudicate_translation/v1`, scorer, provider/model, temperature, output cap,
languages, category, both spans, and the complaint. It never contains the API
key. `--api-key-env` is an environment-variable **name**, never a key value.
Parsed and unparseable responses are both cached; an unparseable response is a
terminal JSONL record and is never repaired or re-prompted.

The frozen adjudication JSONL schema records identity and audit text plus:

```
schema_version, prompt_version, finding_id, passage_id, finding_index,
category, source_quote, translation_quote, explanation, status,
verdict, confidence, rationale, cached, input_tokens, output_tokens, error
```

`status` is `parsed`, `unparseable`, or `error`; parsed `verdict` is
`true_positive`, `false_positive`, or `unclear`. The summary emits all six
categories with `findings`, `adjudicated`, verdict counts, `unparseable`,
`request_errors`, and `true_positive_rate`. The rate is true positives divided
by parsed adjudications, including parsed `unclear` verdicts. Unparseable
responses and request errors are visible but do not become false positives. A
category with no parsed adjudications has JSON `null` and prints `-`, because
0/0 is not 0% precision.

### Measured precision, 2026-07-29 — and why one number is not enough

The same **210 findings** (The Cyberiad, arm A, judged by deepseek-v4-pro) were
adjudicated twice:

| category | n | v4-pro adjudicating (**self**) | Kimi K3 adjudicating |
| --- | --- | --- | --- |
| meaning_changed | 155 | 66.5% | **23.2%** |
| target_language_error | 36 | 58.3% | **16.7%** |
| content_dropped | 15 | 26.7% | **0.0%** |
| register_shift | 4 | 100% | 75% |
| overall | 210 | ~63% | **~21%** |

87 of 210 verdicts differ. 75 are v4-pro calling true-positive where Kimi calls
false-positive.

**Never let a model adjudicate findings it produced.** v4-pro rated its own
output roughly three times more favourably than an independent model did. Always
cross-model, and treat a self-adjudicated rate as meaningless rather than
optimistic.

**On this evidence v4-pro is a weak judge for translation quality.** Five
disagreements were read by hand and Kimi was right in all five. The clearest:
`Royal Appropriations Commission` → `Commissione Reale per gli Stanziamenti`,
where v4-pro's rationale asserts the translation "adds Reale" — while *Royal* is
in the source. That is the same self-contradiction this document already records
for v4-pro on validator flags. The others were pedantic rather than wrong:
objecting that `go dead` → `morire` overreaches when the subject is a living
beast, or that `the King wouldn't pay` → `non voleva pagare` shifts nuance, when
that is exactly how Italian expresses volitional refusal.

**Consequence for reading any rate.** Discount a v4-pro-judged run hard. The
Cyberiad A/B reported ~10–12 hard defects per 1,000 words; at roughly 23%
precision the genuine rate is nearer 2–3. This does not change that A/B's
conclusion — no significant difference between arms — it widens the noise floor.

A hand review of eight findings by the person who framed the experiment is not a
measurement either. That review called roughly half genuine; read against an
independent adjudicator's counter-arguments it was too generous.

**`--limit` truncates in input order, it does not sample** — the same trap
recorded above for `judge_flags`, and it defaults to 25. Use `--limit 0`, or
shuffle with a recorded seed first.

## What this measured, 2026-07-26/27

Recorded here so the next person does not re-derive it. Corpus: 29 real
English→Italian jobs, deepseek-v4-flash.

| | flags |
| --- | --- |
| protected-span flags, before any fix | 600 |
| after marker-aware span detection (#61) | 371 |
| after severity demotion (#64) | 348 |
| after the OCR letter-run guard (#65) | 265 |
| after localized numeric word-suffix handling | 172 |

False-positive rate over that period stayed roughly flat, **75.3% → 77.5%**.
That is the important finding: four rounds of heuristic tuning cut *volume*
without improving *precision*. It is why protected-span violations were demoted
to warnings rather than tuned a fifth time, and why further heuristic work there
is not recommended without new evidence.

Two things the measurement surfaced that nobody was looking for:

- **~29% of remaining false positives were OCR damage in the source**, not
  validator logic. A `^` standing in for a misread letter is a strong inline
  math operator, so garbled prose became a protected "math" span. When flag
  volume spikes, suspect the scan before the validator.
- `other` and `unknown_inline_marker` came back **100% true positive** in both
  runs (small n). Those checks earn their keep and remain hard failures.

## The LLM QA pass, measured

`--qa` sends translated segments to a model for review. It is worth having: on
one book it caught the book's own title `The Cyberiad` rendered as
`Il Ciberspazio` ("cyberspace"), which no deterministic check can find.

Judged by the same method as above — same book, same segments, deepseek-v4-pro
adjudicating — it runs at roughly **40% true positive against 3.2% for the
deterministic protected-span check**. It is the more precise of the two signals
by an order of magnitude, and worth calibrating rather than removing.

### Three prompt variants, all measured

Do not repeat these. Same book, same nine segments, same judge:

| prompt | findings | true positive |
| --- | --- | --- |
| "Do not nitpick harmless style choices" | 15 | **40.0%** |
| plus explicit `high`/`medium`/`low` definitions | 54 | 5.6% |
| plus "report only `medium` and `high`" | 99 | 17.5% |

**Every attempt to constrain the model increased its output.** Defining `low` as
"a specific, defensible improvement would fix a minor imprecision" created a
sanctioned category for the noise it was meant to suppress, and 50 of 54
findings arrived as `low`. Removing `low` from the request did not reduce
reporting — the model relabelled all 99 findings `medium`, several of which
state in their own text "This is correct. No issue."

The conclusion is that **severity instructions do not control this model's
output volume or precision**. It reports what it notices on a linear pass and
applies whichever label it is permitted to use. The original wording is in the
tree because it measured best, not because it is well written.

### Why: review units are the wrong size

Segment sizes on a real book are extreme and bimodal:

```
segments 32   median 12 chars   max 56,106 chars   ten over 10k
```

The median review unit is a table-of-contents line; the largest is 56,000
characters. Segments are sized for *translation* throughput, where large
batches are efficient, and QA inherited that sizing. Output volume tracks input
size regardless of what the prompt asks for, which explains all three rows above
better than any theory about wording.

This also explains a specific miss. In a 15,000-character segment a machine is
built that produces anything beginning with `n`; the translation has it make
`aghi` and `inchiodare`, neither of which begins with n in Italian. The rule and
its violation are in the same paragraph and the model did not notice, while
catching terminology drift between two nearby mentions in the same pass. The
review is phrase-by-phrase, not holistic.

### The trap in the obvious fix

Chunking QA into smaller units would focus attention — and would probably
destroy what currently works. Every true positive was a **cross-reference**
error: a term translated in one place and left in English later
(`Steelypips`), one word rendered two ways (`Incursione` / `sortita`), a letter
dropped from an invented word (`CYPHROEROTICON`), register collapsing between
two mentions. Those are only visible when both mentions sit in the same review
unit.

Naive chunking therefore trades a 40% signal for better-behaved noise. Measure
before believing either direction.

## Prevent terminology drift before translation

The terminology defects above share a cheaper intervention point than QA:
extract repeated or invented source terms once, have a strong model propose a
canonical target rendering with a short source excerpt, then require a human
decision before that rendering becomes active.

```bash
bookforge glossary extract-candidates book.epub \
  --book-id cyberiad \
  --source-lang English \
  --target-lang Italian
bookforge glossary propose book.epub \
  --book-id cyberiad \
  --language "English->Italian" \
  --qa-provider openrouter \
  --qa-model moonshotai/kimi-k3
bookforge glossary review-candidates cyberiad --language "English->Italian"
```

The proposal prompt makes invented-word strategy explicit: preserve the source,
translate it directly, calque its components, recreate a target-language
neologism, or decline when a genuine term's excerpt is insufficient. It can
separately reject ordinary language as `not_terminology`. Every answer requires
a one-sentence rationale.

Renderings and model rejections remain inactive `auto_candidate` rows; the
reviewer must accept or edit them before they can reach translation. A model
rejection is stored with a visible `model rejection (not terminology): ...`
note, is skipped by later proposal passes, and remains in `review-candidates`
for human override. Human rejection uses the distinct `rejected` status. The
command reports how many candidates the model rejected and prints each reason.
Settled and already-proposed terms are not sent again. The proposal pass uses
bounded chunks of at most 25 candidates by default (8192 output tokens,
budgeted at 320 per candidate). Truncated or structurally invalid responses are
bisected and retried down to a single candidate. Completed chunks are retained,
but any terminal chunk failure makes the command print an `INCOMPLETE` summary
with completed and failed counts and exit with an error; failed candidates
remain pending.

Candidate extraction itself is English-specific: its positional evidence models
English capitalization and non-English input gets only a 17-word English
fallback stoplist. German common nouns are capitalized mid-sentence, so every
repeated noun can clear that filter. Treat extraction as recall and the strong
model as precision. The default `--min-count 3` is the recommended compromise:
it recovers three-occurrence terms without paying the 320-output-token budget
for the much larger one- and two-occurrence tail.

This pass is deliberately book-scoped rather than segment-scoped. A measured
run — The Cyberiad, 40 candidates, Kimi K3 — cost 2,355 provider input tokens and
**8,277 output tokens**: about 207 output tokens per candidate, most of it
reasoning. That is cents per book, once, against paying for repeated QA after
terminology has already drifted. The command prints both its prompt estimate and
provider-reported usage; use the selected model's current rates for budgeting.

### Reproducible accepted-glossary A/B

This experiment asks whether an accepted glossary reduces translation defects,
not merely whether the glossary machinery runs. **Never run it against the
owner's real store.** That store contains 30 irreplaceable jobs. BookForge opens
`.bookforge/jobs.sqlite` relative to the process working directory, so every
command below runs from one newly created scratch directory and every resulting
store, run snapshot, output, judge result, and cache stays under that directory.

The final commands require the `judge_translation` example from the quality
benchmark work. Use a checkout containing both that example and the glossary
batch-accept command. In PowerShell, edit only `$Repo` and `$Book`, then run the
commands in order:

```powershell
$Repo = (Resolve-Path "C:\path\to\bookforge").Path
$Book = (Resolve-Path "C:\path\to\book.epub").Path
$Scratch = Join-Path ([IO.Path]::GetTempPath()) ("bookforge-glossary-ab-" + [guid]::NewGuid())
$Manifest = Join-Path $Repo "Cargo.toml"
$BookId = "glossary-ab-book"
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:LOCALAPPDATA\mingw64\bin;$env:Path"
New-Item -ItemType Directory -Path $Scratch | Out-Null
Set-Location $Scratch

# Offline preflight: estimate one translation before spending anything.
cargo run --manifest-path $Manifest --package bookforge-cli --release --bin bookforge -- `
  estimate $Book --source English --target Italian `
  --provider deepseek --model deepseek-v4-flash

# Paid run A: no accepted glossary exists in this new store.
cargo run --manifest-path $Manifest --package bookforge-cli --release --bin bookforge -- `
  translate $Book --source English --target Italian `
  --provider deepseek --model deepseek-v4-flash --no-thinking `
  --book-id $BookId --out (Join-Path $Scratch "baseline.it.epub")

# Copy the value printed after "Job:" before continuing.
$BaselineJob = "<baseline-job-id>"

# Extraction is offline. Proposal is paid and remains inactive.
cargo run --manifest-path $Manifest --package bookforge-cli --release --bin bookforge -- `
  glossary extract-candidates $Book --book-id $BookId `
  --source-lang English --target-lang Italian
cargo run --manifest-path $Manifest --package bookforge-cli --release --bin bookforge -- `
  glossary propose $Book --book-id $BookId --language "English->Italian" `
  --qa-provider deepseek --qa-model deepseek-v4-pro

# Explicit, non-interactive human decision. Require accepted > 0.
cargo run --manifest-path $Manifest --package bookforge-cli --release --bin bookforge -- `
  glossary accept-candidates $BookId --language "English->Italian"

# Paid run B: identical translation settings, now with accepted book terms.
cargo run --manifest-path $Manifest --package bookforge-cli --release --bin bookforge -- `
  translate $Book --source English --target Italian `
  --provider deepseek --model deepseek-v4-flash --no-thinking `
  --book-id $BookId --out (Join-Path $Scratch "glossary.it.epub")

# Copy the second value printed after "Job:".
$GlossaryJob = "<glossary-job-id>"

# Offline judge preflights: all passages, identical judge and settings.
cargo run --manifest-path $Manifest --package bookforge-cli --release `
  --example judge_translation -- --db (Join-Path $Scratch ".bookforge\jobs.sqlite") `
  --job $BaselineJob --sample 0 --dry-run `
  --provider deepseek --model deepseek-v4-pro --max-output-tokens 16000
cargo run --manifest-path $Manifest --package bookforge-cli --release `
  --example judge_translation -- --db (Join-Path $Scratch ".bookforge\jobs.sqlite") `
  --job $GlossaryJob --sample 0 --dry-run `
  --provider deepseek --model deepseek-v4-pro --max-output-tokens 16000

# Paid judging. The second summary also prints per-category baseline deltas.
cargo run --manifest-path $Manifest --package bookforge-cli --release `
  --example judge_translation -- --db (Join-Path $Scratch ".bookforge\jobs.sqlite") `
  --job $BaselineJob --sample 0 `
  --provider deepseek --model deepseek-v4-pro --max-output-tokens 16000 `
  --cache (Join-Path $Scratch ".bookforge\translation-judge-cache") `
  --out (Join-Path $Scratch "judge-baseline.jsonl") `
  --summary (Join-Path $Scratch "judge-baseline.summary.json")
cargo run --manifest-path $Manifest --package bookforge-cli --release `
  --example judge_translation -- --db (Join-Path $Scratch ".bookforge\jobs.sqlite") `
  --job $GlossaryJob --sample 0 `
  --provider deepseek --model deepseek-v4-pro --max-output-tokens 16000 `
  --cache (Join-Path $Scratch ".bookforge\translation-judge-cache") `
  --baseline (Join-Path $Scratch "judge-baseline.summary.json") `
  --out (Join-Path $Scratch "judge-glossary.jsonl") `
  --summary (Join-Path $Scratch "judge-glossary.summary.json")
```

Do not continue if batch acceptance prints `accepted=0`; there would be no
glossary treatment to measure. It always reports all outcomes in one stable
line, for example:

```text
Bulk acceptance: accepted=37 skipped-empty=1 skipped-model-rejected=2.
```

Compare the `hard` group's `per_1k_source_words` in the two summary files.
Also require equal `passages_judged` and `source_words_judged` before treating
the rates as a paired comparison. Use the same judge, output cap, passage size,
and sample/seed for both jobs; `--sample 0` above removes sampling variance.

The second translation gets **no translation-cache hits by design** once at
least one term is accepted. Active glossary content changes its fingerprint,
and that fingerprint changes the cache namespace. Run B therefore costs full
price rather than being a nearly free cache replay.

For a realistic translation budget, start with the offline `estimate` command
above and double its one-run figure. As a concrete scale, 100,000 estimated
input tokens and BookForge's 1.15× Italian output assumption produce 115,000
output tokens. At the bundled and current
[DeepSeek V4 Flash rates](https://api-docs.deepseek.com/quick_start/pricing/)
of $0.14/M uncached input and $0.28/M output, that is about **$0.046 per run or
$0.092 for the two translations**. Prompt/context overhead, retries, and the
second run's glossary block make **$0.10–$0.15 for the translation pair** a
more practical one-book budget. Proposal and judge calls are separate; both
commands report usage or a dry-run maximum before the owner approves payment.

**Bound each request while keeping the per-candidate allowance honest.** A
reasoning model that runs out mid-thought returns HTTP 200 with *no message
content at all* — not a truncated answer — so the failure used to surface as
`missing choices[0].message.content`, which says nothing about the cause. This
happened four separate times in two days: `judge_flags` at its 400-token
default, the QA pass at the provider default, this command at a flat 4,096, and
`judge_translation` — which starved on 10 of 25 passages at a 4,096 cap and
still on 2 of 25 at **16,000**.
The provider now recognises that shape — `finish_reason: length`, or reasoning
present with content absent — and names the relevant flag. `glossary propose`
caps each request at 8,192 output tokens by default and sizes chunks at 320
tokens per candidate. It also bisects a truncated or invalid chunk rather than
discarding the entire proposal pass.

## Cost

Judging 389 flags cost about $0.07 on deepseek-v4-flash, $0.21 on
deepseek-v4-pro, and roughly $1.72 on Kimi K3 before cache hits. The entire
two-day measurement effort cost under $3. A full QA pass over one book with
Kimi K3 is about $0.35.

Add new models to `pricing/providers.json` **and** the packaged copy at
`crates/bookforge-cli/pricing/providers.json` — a test asserts they are
identical.
