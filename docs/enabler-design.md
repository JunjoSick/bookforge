# The enabler — design notes

A component that prepares and supervises a translation run: it inspects the
book and the provider, chooses settings, watches the run, and adapts when
something goes wrong.

This document records both the agreed shape and the deliberately narrow first
slices. `bookforge plan` implements read-only inspection, and `bookforge
translate --plan` can now apply its actionable recommendations when a job is
created. Supervision remains future work; plan application is deliberately
opt-in.

## What it is, and deliberately is not

The enabler is a **deterministic planner** over measurable inputs. A model is
consulted only where rules genuinely run out, or where a run has gone wrong in a
way the rules do not recognise.

That choice is not timidity, it is what the evidence supports. Every setting the
enabler needs to choose is a function of things that can be *measured* — the
script of the source text, the block-size distribution, the provider, the
observed failure signature. None of them requires judgement about meaning. A
rules engine is cheaper, testable offline, reproducible, and cannot invent a
setting that does not exist.

**It must not touch translations.** ROADMAP §1.1 is explicit: the model never
sees or repairs raw XHTML, and a malformed response is rejected and retried,
never sent to a repair model. The enabler chooses *how the work is run*. It has
no authority over *what the output says*. Any future proposal that has an agent
inspect and amend translations is a different feature and must be argued against
§1.1 on its own terms.

## Why it is worth building

Because the settings demonstrably matter more than the model. Measured on
《矛盾论》, Chinese to Italian:

| | default settings | tuned settings |
| --- | --- | --- |
| `deepseek-v4-flash` | **61 of 211 blocks** — 71% of the book silently lost | 211 of 211, zero failures |

The only difference was `--batch-target-tokens 800 --batch-max-items 4`. No user
would guess those. The run reported a plausible-looking summary either way.

## What it decides, and the evidence for each

| decision | inputs | evidence it is needed |
| --- | --- | --- |
| batch target tokens, max items | source script, block-size distribution | the table above |
| batch max output tokens | estimated output per item | the oversized-response cliff: three of five models failed the same two segments |
| provider output cap | per-item output estimate, model class | five empty-content starvations across four call sites in one day |
| thinking suppression | provider identity | the wrong parameter went to every provider until it was made provider-aware |
| concurrency | observed latency, 429 rate | existing telemetry already reports p50/p95 and 429 counts |
| glossary on or off | whether an accepted glossary exists | a measured A/B found no detectable quality effect, so this should default **off** |

Model choice is deliberately **not** in the enabler's remit for now. Price does
not predict quality, two judges disagreed on ordering, and the only way to choose
well is a measurement we do not yet trust. Recommending a model is documentation
work, not automation.

## Two halves

### Planning, before the run

Read-only inspection, no provider calls, no cost:

1. **Script.** Follow the precedent in `candidate_extraction_strategy` — route on
   the dominant script of the text itself, not a language-name argument. It
   generalises to languages nobody enumerated and does not trust `--source`.
2. **Size distribution.** Median, p90 and max block size, and the same for
   segments. The failures that matter come from the tail, not the mean: on one
   book the third-largest segment was 19 blocks and the largest two were 45 and
   62.
3. **Provider.** Which suppression parameter is honoured, what the output ceiling
   is, whether caching applies.
4. **Prior runs.** If this book has been translated before, start from what
   worked rather than from defaults.

Output is a settings recommendation **with a reason attached to each value**. A
planner that cannot explain itself is not auditable, and every hard-won number in
this project came from someone asking "why is it that?".

### First slice: `bookforge plan`

The first slice resolves two earlier open questions conservatively: it is a
separate command, and it advises without applying settings. It reads an EPUB and
emits human-readable recommendations or stable, schema-versioned JSON. It never
constructs a provider, makes a network request, starts translation, changes the
EPUB, or creates `.bookforge/` state.

Its current rules are:

1. Build scheduler segments with the same default `v1-fast` and built-in target
   sizing policy used by `translate`. Recompute block sizes with the shared,
   script-aware estimator rather than trusting language flags or stored values.
2. Classify the dominant script by counting cased and caseless alphabetic
   characters in translatable text. A tie is undetermined. The declared
   `--source` is reported but never used for sizing.
3. Treat 8,192 output tokens as the response safety boundary: it is the
   power-of-two boundary immediately below the smallest measured failure at
   roughly 9,000 tokens. Generic estimated translated output is 1.15 times
   source tokens, consistent with `estimate`, plus the executor's 128-token
   batch and 64-token-per-item JSON envelopes (and its run-preserving envelope).
4. For cased-script books, keep the `v1-fast` input-token and item defaults. If
   the estimated maximum default batch crosses 8,192, recommend
   `--batch-max-output-tokens 8192`; the executor uses it during packing, so this
   splits the tail without globally multiplying prompt overhead.
5. For caseless-script books, derive a density guard from the inspected text:
   `ceil(4 * estimated source tokens / source characters)`, clamped to 1 through
   4. Divide the response budget by that guard; fit as many p90 estimated-output
   items as remain after the fixed JSON envelope; then set the token target to
   the minimum of the profile default, the guarded output capacity, and p90
   source tokens times that item count. Round the result down to a 256-token
   step. This produced 768 tokens and 3 items on the measured Chinese book; the
   numbers are consequences of its distribution, not copied from the successful
   800/4 experiment.
6. Make the provider output ceiling explicit: the current executor permits
   32,768 tokens for DeepSeek and model names it recognizes as reasoning, and
   16,384 otherwise. Suppress thinking only for provider identities with a
   parameter the current provider code recognizes. Leave glossary injection off
   because the measured A/B found no detectable quality effect.

The general design calls for prior-run reuse. That part is not in the first
slice because the current `JobStore::open` path creates/migrates state; calling
it would contradict the command's read-only boundary. Plans say explicitly that
no prior evidence was consulted. Reuse needs a genuinely read-only store API or
an explicit state input before it can be added safely.

### Second slice: opt-in application at job creation

`bookforge translate --plan` consumes the same typed `Plan` used by the
read-only command. The flag fits the existing command surface: it names the
already-documented operation, remains a simple opt-in, and avoids a second set
of planner-specific tuning flags.

Precedence is field-specific and unambiguous:

1. direct setting flags (`--batch-max-items`, `--max-output-tokens`, and so on)
   always win;
2. actionable plan recommendations fill only fields without a direct flag;
3. profile, target-style, and provider-preset resolution supplies the baseline.

The current consumer applies batch target tokens, batch max items, an optional
batch output bound, the provider output budget, and recognized thinking
suppression. It does not rewrite recommendations whose disposition is merely
"keep default" or "omit". Every applied value and its original planner reason
is captured in the run snapshot at `finalize.applied_plan`, including the plan
schema version.

Planning runs after the EPUB has been parsed but before provider construction
and the translation's final segmentation. The planner needs the size
distribution produced by its default scheduler segmentation; it builds that
inspection from the in-memory `Book`, then the translation builds final
segments with the applied settings. This repeats a cheap segmentation pass but
does not parse the EPUB archive twice and does not contort the normal no-plan
path.

Application is creation-only. `resume` treats the run snapshot as authoritative
and never reruns rules that may have changed between BookForge revisions. The
cache namespace hashes segmentation, profile, whether batching is enabled,
prompt version, and prompt-input fingerprints; it does not hash the applied
batch target/item bounds, output budgets, or thinking suppression. The current
plan is therefore cache-namespace-safe, but rerunning it mid-job would still
make one book use two planner revisions without an operator decision.

`reconfigure` composes on top: its supported cache-safe settings supersede the
captured baseline for remaining work and are merged durably into the snapshot.
The initial plan rationale remains present, so a later reader can distinguish
why the job started with a value from a deliberate runtime change.

The flag should become default-on only after a versioned rule set has been
validated across substantially more books, scripts, providers, and models, and
shows repeatable improvement in blocks recovered, failed requests, or cost per
1,000 source characters without regressions on existing workloads.

### Supervision, during the run

A failure-signature table, built from signatures already observed:

| signature | remedy |
| --- | --- |
| `error decoding response body` | retry, then bisect the batch |
| `max_output_tokens limit reached` | raise the cap, then bisect |
| `the model produced no content` | raise the cap — reasoning consumed the budget |
| `batch translation block mismatch` | bisect |
| HTTP 429 | back off, reduce concurrency |
| p95 latency far above p50 | reduce concurrency |

Much of this already exists inside the batch executor. The enabler's contribution
is to make it **adaptive across the whole run** rather than per-request, and to
**persist what it learned** so the next run of the same book starts correctly.

Escalation to a model happens only when a signature is **unrecognised**. That
boundary must be explicit in the code, not emergent.

## Where a model genuinely earns its place

Only two places are visible so far, and both are advisory:

- **Register and style.** "This is philosophy, prefer a formal register, keep
  coined terms consistent." Rules cannot infer that from block sizes. Note the
  honest caveat: the glossary A/B suggests context injection of this kind may not
  measurably help, so this should be built to be *measured*, not assumed.
- **Unrecognised failures.** When the signature table misses, a model reading the
  error and the run state may suggest something a rule would not. Its output is a
  *suggestion to a human or to a bounded action set*, never an arbitrary command.

## How we will know it works

This is the part the project has repeatedly got wrong, so it is stated up front.

The obvious metric — "did quality improve?" — is currently unmeasurable. The
quality benchmark runs at roughly 30% precision, two judges ranked the same six
translations differently, and one book is about 10x underpowered for a close
comparison.

So the enabler is **not** justified on quality. It is justified on **completion
and cost**, both of which are measured reliably today:

- **blocks recovered / blocks expected** — 61/211 against 211/211 is not a subtle
  effect and needs no judge
- **failed requests per run** — billed but unrecorded, so failures cost real money
- **cost per 1,000 source characters** — characters, not words, because
  whitespace word counting is wrong on unspaced scripts

If the enabler cannot move those, it is not working, and no amount of plausible
architecture should persuade us otherwise.

## Remaining open questions

- **How much does persistence remember?** Per book, per book-and-provider, or a
  global learned profile. Global learning across unrelated books is the kind of
  thing that looks clever and is impossible to debug.
- **When has opt-in earned default-on?** The minimum evidence is broader,
  versioned validation across scripts/providers/models with completion and cost
  gains and no material regressions; three books and one-day-old rules are not
  enough.

## Not in scope

Multi-agent translation. A swarm of specialists — subject expert, philosopher,
reviewer — is a separate and much larger proposal. It cannot be evaluated today
for the same reason quality cannot: we cannot reliably tell two translations
apart. It should wait for a trustworthy measurement, which currently means a
hand-labelled gold set.
