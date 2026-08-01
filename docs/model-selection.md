# Choosing a translation model

Operational guidance from measured runs on this repo's own tooling, not from
vendor claims or public leaderboards.

**Read this first: reliability and cost are measured and solid. The quality
ranking is not established.** Two independent judges produced different
orderings of the same six translations, so this document reports what survives
that disagreement and is explicit about what does not.

## Short answer

| Need | Model | Why |
| --- | --- | --- |
| Default, and now also for bulk | `openai/gpt-5.6-luna` | Completed everything first try in both English and Chinese, no fluency complaints, and since the 2026-07-31 price cut it is **cheaper than deepseek-v4-flash on input** |
| Cost floor, if you tune it | `deepseek/deepseek-v4-flash` | Still cheapest on output and has an 80% input-cache discount, but needs batch tuning on non-Latin scripts and drops content far more often |
| Avoid for translation | `anthropic/claude-opus-5` | The one quality claim that survived both judges: consistently in the worse group, at roughly 19x Luna's cost, and it never completed the test slice |

**Price does not predict quality.** Across six models spanning a 600x price
range, the two most expensive placed first and last under one judge, and swapped
under the other.

**The Flash price argument has largely evaporated.** On 2026-07-31 Luna moved
from $0.50/$3.00 to **$0.10/$0.60** per million. An earlier version of this
document recommended Flash for bulk on a 25x gap; measured on a real Chinese book
the two now cost **$0.0256 against $0.0225** -- about 12% apart -- because Luna is
cheaper on input and Flash needed small batches, which multiplied its output
tokens to 64k against Luna's 36k. Output is exactly where Flash's advantage is
thinnest (0.28 against 0.60, roughly 2x, against being *more* expensive on
input).

Flash remains genuinely cheap where **input dominates and caching applies** --
resumes, retries, re-runs over mostly-cached content, where its cached input
falls to $0.028/M. Judge that per workload rather than from the headline rate.

## What was measured

Six models translated one identical 47,705-character slice of dense literary
prose (English to Italian), chosen by measuring density of invented vocabulary
across the whole source book. Costs are provider-dashboard figures, not
projections.

### Reliability — the most decision-relevant result

Two segments in the slice needed roughly 9,000 and 11,000 output tokens in a
single response. **Three of six models failed both**, from three different
vendors, with an identical transport decode error — losing 73% of the book's text
while the run still reported a plausible-looking summary.

| model | default batching | after `--batch-max-items 12` |
| --- | --- | --- |
| `deepseek/deepseek-v4-flash` | **165/165 blocks** | — |
| `openai/gpt-5.6-luna` | **165/165** | — |
| `openai/gpt-5.6-terra` | **165/165** | — |
| `x-ai/grok-4.5` | 75/165 | 165/165 |
| `anthropic/claude-fable-5` | 75/165 | 165/165 |
| `anthropic/claude-opus-5` | 74/165 | 164/165 |

Opus-5 never fully recovered: one block returned HTTP 200 with no content even
bisected to a single item.

Batch retry and bisection now handle this automatically. For a book with very
large chapters on a model in the lower group, an explicit
`--batch-max-output-tokens` near 8,000 splits only the oversized shapes.
**Do not shrink batches globally** — smaller batches re-pay prompt overhead per
request and cost roughly 2.7x more.

### Cost

| model | $/M in · out | actual for the slice | $ / 1k source words |
| --- | --- | --- | --- |
| `deepseek/deepseek-v4-flash` | 0.14 / 0.28 | **$0.01** | 0.002 |
| `openai/gpt-5.6-terra` | 1.25 / 7.50 | $0.21 | 0.049 |
| `openai/gpt-5.6-luna` | 0.50 / 3.00 | $0.25 | 0.058 |
| `x-ai/grok-4.5` | 2.00 / 6.00 | $1.30 | 0.306 |
| `anthropic/claude-opus-5` | 5.00 / 25.00 | $4.81 | 1.131 |
| `anthropic/claude-fable-5` | 10.00 / 50.00 | $6.12 | 1.439 |

Two budgeting traps:

- **A run with failures cost more than it reports.** A failed request is billed
  but records no tokens. The two models with zero failures reconciled to the
  cent against the provider dashboard; the two that lost segments were
  under-reported by 53% and 154%.
- **Catalog prices are a ceiling, not the charge.** One model billed at 41% of
  its listed rate. Verify against the provider dashboard.

### Quality — reported, not ranked

Judged on the passages common to every arm, so exposure is equal. Under
`deepseek-v4-pro` (38 passages, 4,253 words), ordered by residual hard defects
after removing wordplay-handling complaints and the judge's own self-refuting
findings:

| model | residual | hard/1k | soft/1k |
| --- | --- | --- | --- |
| `x-ai/grok-4.5` | 35 | 12.5 | 5.4 |
| `anthropic/claude-fable-5` | 50 | 13.4 | 1.4 |
| `openai/gpt-5.6-luna` | 61 | 16.5 | **0.5** |
| `openai/gpt-5.6-terra` | 88 | 22.8 | 7.1 |
| `deepseek/deepseek-v4-flash` | — | 24.5 | 6.6 |
| `anthropic/claude-opus-5` | 97 | 25.6 | 0.5 |

DeepSeek Flash's weakness is specific: **17 `content_dropped` findings** against
Luna's 5 and Terra's 1. It is the likeliest of the six to omit something.

**Then a second judge disagreed.** `x-ai/grok-4.5` re-judged the same
translations and produced a different ordering:

```
deepseek-v4-pro : Fable 5  < Terra < Grok 4.5 < Opus 5 < Luna
grok-4.5        : Opus 5   < Luna  < Fable 5  < Grok 4.5 < Terra
```

Luna is last under one judge and second under the other. Worse, **Luna and Terra
swap order between grok's own two views** of its own data, so grok cannot
separate them either.

What survives both judges:

- **Opus 5 is in the worse group.** The only quality claim with agreement.
- **Grok 4.5 looks best under both** — but under grok that is a model judging its
  own translation, which is known to inflate. Discount it.
- **Luna against Terra against Fable 5 is unresolved.**

## Non-Latin scripts need batch tuning, and the benchmark cannot score them yet

Measured on 《矛盾论》 (*On Contradiction*, 27,412 characters, Chinese to Italian).

**Default batching fails badly.** BookForge's token estimators are
whitespace- and Latin-shaped, so they under-count languages without inter-word
spaces and the planner packs batches several times larger than it believes:

| model, default batching | segments | blocks recovered |
| --- | --- | --- |
| `openai/gpt-5.6-luna` | 12/15 | 208 of 211 |
| `deepseek/deepseek-v4-flash` | 8/15 | **61 of 211 — 71% of the book lost** |

Luna's failures were output-cap truncation; Flash's were
`HTTP error: error decoding response body`, the oversized-response cliff.
`--batch-target-tokens 800 --batch-max-items 4` recovered Flash **completely**
(211 blocks, zero failures), which is the proof the planner is mis-sizing rather
than the model being weak.

**Until token counting is script-aware, set batch sizes explicitly for CJK
sources.** `bookforge estimate` also under-reports these languages badly — it
predicted 446 input tokens for a book that consumed 37,362.

**Quality on Chinese, on 22 identical passages (18,865 source characters):**

| | hard defects | of which content dropped |
| --- | --- | --- |
| Luna | 142 | 9 |
| Flash | 134 | **49** |

Totals are indistinguishable (p = 0.67), but the failure *shapes* differ:
**Flash drops content 5.4x more often**, echoing the same weakness on the
English slice (17 against 5). Silent omission is the worse failure mode — a
reader never learns something is missing, whereas a changed nuance is visible.

Treat this loosely. The judge is unvalidated on Chinese, and `content_dropped` is
its weakest category at 13.3% precision — though it is suggestive that the same
asymmetry appears independently on two books in two languages.

**Rates are not reported for Chinese** because `judge_translation` derives
"source words" by whitespace splitting, which inflates every per-1,000-word
figure by roughly 100x on unspaced scripts. Only raw counts on identical
passages are meaningful here.

## Limits — read before quoting any of this

- **The judge is ~30% precise.** Measured by majority vote of three independent
  adjudicators over 210 findings: `meaning_changed` 29.0%,
  `target_language_error` 38.9%, `content_dropped` **13.3%**. Absolute rates are
  inflated roughly 3x. `content_dropped` is nearly seven-eighths false and should
  be read with heavy scepticism.
- **Never let a model adjudicate its own findings.** One model rated its own
  output 66.5% true-positive where independent adjudicators said 23–43%.
- **A single judge cannot be neutral about style.** One flags
  `cyberivy-bushwhacker` rendered as `sterpaciberedera` as a meaning change —
  which is what deliberate coinage-recreation looks like.
- **Judge coverage is uneven.** Grok lost 34 of 61 passages on one arm and 24 on
  another to empty responses, which is why the two-judge overlap is small.
- **38 passages, 4,253 words, one book** — and the hardest book in the corpus.
  Corpus-wide measurement over 300 passages of eight books put hard defects at
  5.9/1k against this slice's 10–25. These are stress-test figures.
- **Model identities and prices move.** Re-run before relying on any of this.

## Reproducing

The method is in `validator-tooling.md`. In outline: translate the same input
with each model into a scratch store, judge with `examples/judge_translation`,
compare only passages common to every arm, and calibrate with
`examples/adjudicate_translation` using **two or more independent** adjudicators.

Report a p-value, not a percentage. At roughly 170 findings per arm the noise
floor is about ±19, so only effects above ~20% are detectable — one book cannot
settle a close comparison.
