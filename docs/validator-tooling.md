# Measuring validator quality

BookForge flags a translated segment as `needs_review` when a deterministic,
string-level check fires. Those checks have no model of meaning, so some
proportion of what they flag is wrong — and until 2026-07-26 nothing measured
which proportion.

Two dev-time examples exist to answer that. Neither is wired into `translate`,
and neither should be: a model that gated or repaired translations would violate
the architectural invariants in `ROADMAP.md` §1.1–§1.2.

```
cargo run --release --example replay_validation -- --help
cargo run --release --example judge_flags -- --help
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
- `--emit-pairs <file.jsonl>` writes the flagged pairs for the judge below.

Use it to answer "did this validator change help?" before spending anything.

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
  described under "Size the output cap from the candidate count" below, and it
  has now been hit by three separate call sites.
- **Verdicts are cached on disk** by content hash, so re-runs of already-judged
  units are free. Keep the cache directory between runs.
- **Judges disagree, and the stricter one has been right.** On a shared subset,
  deepseek-v4-pro reported 97.3% false positive against Kimi K3's 82.4%. v4-pro
  was caught contradicting its own rationale and dismissing real defects — a
  source `December 8` rendered as `10 dicembre`, which is silent data
  corruption. Treat a single judge's rate as a bound, not a measurement, and
  prefer the stricter model.

## What this measured, 2026-07-26/27

Recorded here so the next person does not re-derive it. Corpus: 29 real
English→Italian jobs, deepseek-v4-flash.

| | flags |
| --- | --- |
| protected-span flags, before any fix | 600 |
| after marker-aware span detection (#61) | 371 |
| after severity demotion (#64) | 348 |
| after the OCR letter-run guard (#65) | 265 |

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
Settled and already-proposed terms are not sent again, and a provider or
response-validation failure occurs before any candidate write.

Candidate extraction itself is English-specific: its positional evidence models
English capitalization and non-English input gets only a 17-word English
fallback stoplist. German common nouns are capitalized mid-sentence, so every
repeated noun can clear that filter. Treat extraction as recall and the strong
model as precision. The default `--min-count 3` is the recommended compromise:
it recovers three-occurrence terms without paying the 320-output-token budget
for the much larger one- and two-occurrence tail.

This pass is deliberately book-sized rather than segment-sized. A measured run —
The Cyberiad, 40 candidates, Kimi K3 — cost 2,355 provider input tokens and
**8,277 output tokens**: about 207 output tokens per candidate, most of it
reasoning. That is cents per book, once, against paying for repeated QA after
terminology has already drifted. The command prints both its prompt estimate and
provider-reported usage; use the selected model's current rates for budgeting.

**Size the output cap from the candidate count, not a flat number.** One request
carries every pending candidate, so the budget has to scale. A reasoning model
that runs out mid-thought returns HTTP 200 with *no message content at all* —
not a truncated answer — so the failure used to surface as
`missing choices[0].message.content`, which says nothing about the cause. This
happened three separate times in two days: `judge_flags` at its 400-token
default, the QA pass at the provider default, and this command at a flat 4,096.
The provider now recognises that shape — `finish_reason: length`, or reasoning
present with content absent — and names the flag to raise. `glossary propose`
sizes its own budget at 320 tokens per candidate (floor 8,192, ceiling 65,536).

## Cost

Judging 389 flags cost about $0.07 on deepseek-v4-flash, $0.21 on
deepseek-v4-pro, and roughly $1.72 on Kimi K3 before cache hits. The entire
two-day measurement effort cost under $3. A full QA pass over one book with
Kimi K3 is about $0.35.

Add new models to `pricing/providers.json` **and** the packaged copy at
`crates/bookforge-cli/pricing/providers.json` — a test asserts they are
identical.
