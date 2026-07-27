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
  at 1200. That is a third of the spend wasted.
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

## Cost

Judging 389 flags cost about $0.07 on deepseek-v4-flash, $0.21 on
deepseek-v4-pro, and roughly $1.72 on Kimi K3 before cache hits. The entire
two-day measurement effort cost under $3.

Add new models to `pricing/providers.json` **and** the packaged copy at
`crates/bookforge-cli/pricing/providers.json` — a test asserts they are
identical.
