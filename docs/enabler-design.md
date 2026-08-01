# The enabler — design notes

A component that prepares and supervises a translation run: it inspects the
book and the provider, chooses settings, watches the run, and adapts when
something goes wrong.

**This is a design document, not a specification of shipped behaviour.** Nothing
here is implemented yet. It exists so the shape is agreed before code is written.

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

## Open questions

- **Where does it live?** A pre-flight pass inside `translate`, a separate
  `bookforge plan` command, or a library the CLI and dashboard both call. A
  separate command is easiest to test and easiest to ignore; an integrated
  pre-flight is what actually helps a user who never reads documentation.
- **Does it act, or only advise?** Emitting recommended flags is safe and
  auditable. Applying them automatically is more useful and harder to reason
  about. A middle option — apply, but record every decision and its reason in
  the run snapshot — is probably right, and matches how `reconfigure` already
  works.
- **How much does persistence remember?** Per book, per book-and-provider, or a
  global learned profile. Global learning across unrelated books is the kind of
  thing that looks clever and is impossible to debug.

## Not in scope

Multi-agent translation. A swarm of specialists — subject expert, philosopher,
reviewer — is a separate and much larger proposal. It cannot be evaluated today
for the same reason quality cannot: we cannot reliably tell two translations
apart. It should wait for a trustworthy measurement, which currently means a
hand-labelled gold set.
