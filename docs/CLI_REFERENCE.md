# BookForge CLI guide

This guide explains how the commands fit together. Run `bookforge <command>
--help` for the complete, version-specific option list.

Examples below assume that BookForge is installed. From a source checkout,
replace `bookforge` with `cargo run -p bookforge-cli --`.

## Before you run a command

BookForge stores its job database and run artifacts under `.bookforge/` in the
current working directory. Start later commands such as `status`, `resume`, and
`review` from the same directory where you started the translation. This also
means that two folders can have independent BookForge job libraries.

Provider keys can be pasted into the local dashboard for that server session,
or supplied to CLI commands through environment variables. See
[PROVIDERS.md](PROVIDERS.md) for provider presets, endpoints, and key names.

## Command map

| Command | Purpose |
| --- | --- |
| `serve` | Run the local browser dashboard. Running `bookforge` with no command does the same thing and opens the browser. |
| `inspect` | Report EPUB structure, metadata, and translatable text coverage. |
| `plan` | Inspect an EPUB offline and recommend translation settings with reasons. |
| `estimate` | Estimate input/output tokens and price before a hosted-provider run. |
| `translate` | Start a checkpointed EPUB translation. |
| `watch` | Monitor and control a job in a full-screen terminal UI. |
| `status` | Print persisted progress, settings, token use, artifacts, and performance. |
| `tail` | Print recent durable job events, optionally as JSON. |
| `pause` | Park a running worker at the next safe boundary. |
| `stop` | Ask a running worker to checkpoint and exit at the next safe boundary. |
| `reconfigure` | Change cache-safe scheduling and quality settings for remaining work. |
| `resume` | Continue a paused, stopped, interrupted, or incomplete job. |
| `retry` | Mark failed or review-needed segments as `retry_pending`. |
| `review` | Generate a private, side-by-side HTML review page. |
| `ingest-flags` | Import flags exported by the static review page. |
| `correct` | Save a validated human correction and rebuild the output EPUB. |
| `validate` | Run BookForge structural checks and EPUBCheck when available. |
| `convert` | Convert a PDF into a translation-ready EPUB. |
| `reflow` | Repair paragraph flow in an EPUB without translating it. |
| `glossary` | Manage terminology and extract book-specific candidates. |
| `style` | Manage reusable translation-style guidance. |
| `entities` | Manage names, target forms, gender, and role guidance. |
| `doctor` | Check storage, provider access, PDF tools, or an OCR endpoint. |
| `benchmark` | Measure provider latency and throughput with synthetic requests. |
| `audiobook` | Generate resumable audio chunks and optional stitched audio from an EPUB. |

## Local browser dashboard

The local dashboard is the default front door. Running `bookforge` without a
subcommand starts it on `127.0.0.1:8765` and opens the default browser. The
explicit command uses the same address and 250 ms server-sent-events refresh
interval, but opens a browser only when `--open` is present:

```bash
bookforge
bookforge serve --open
```

Use `--bind` to select another loopback address or port. `--refresh-ms` controls
the live-update interval; values are clamped to the range from 20 through 5,000
milliseconds — the same floor (`MIN_REFRESH_MS`, 20 ms) that
`watch --refresh-ms` uses.

```bash
bookforge serve --bind 127.0.0.1:9000 --refresh-ms 500
```

The dashboard lists jobs and shows their details and live events. It can
estimate and launch translations, review and validate results, save manual
translations, flag or retry segments, and manage glossary entries. It also
routes the reconfigure, pause, resume, and stop controls described below. The
audiobook flow can estimate and launch a run, report its status, cancel it, and
download its artifact. The dashboard is therefore a control surface, not only
a monitor.

Before binding, `serve` checks whether the current working directory can hold
`.bookforge/`. A writable directory is left unchanged. On Windows, an
unwritable directory is replaced with `%LOCALAPPDATA%\BookForge`, falling back
to `%USERPROFILE%\BookForge`. On other platforms it uses
`$XDG_DATA_HOME/bookforge`, falling back to `$HOME/.local/share/bookforge`.
Uploads, job state, outputs, and dashboard-launched child processes then use
that relocated working directory. When the relocation happens, the server
prints `working directory was not writable; storing data in …` on its console.
The CLI has no relocation logic: a job launched in the dashboard is visible to
`bookforge status`, `resume`, and friends only when you run them from the same
directory the server resolved — which is your current directory whenever it was
writable. There is no global override that redirects all BookForge state to one
shared location today, so treat relocation notices as the signal to start other
commands from the printed directory.

The dashboard is authenticated by default. At startup the server generates a
per-session token, prints a bootstrap URL of the form
`http://127.0.0.1:8765/?token=…` on its console, and requires that token on
every route outside that one bootstrap page: API and mutating requests must
carry it in the `x-bookforge-csrf` header, which is also what mutation requests
always used; wrong or missing tokens get a bare 401. The bootstrap URL is the
console equivalent of handing the browser the session — never share or re-print
it (and redact it in screenshots). `--no-auth` disables the token check as an
escape hatch for environments where the console is unreachable (for example a
container orchestrator that only forwards the port); with the token disabled,
any local process can spend remembered provider keys, so prefer an SSH tunnel
plus the default token flow instead.

Because only loopback processes should hold session tokens anyway, `--bind`
rejects every non-loopback address and directs remote users to an SSH tunnel.
Keys pasted into the dashboard remain in server memory only for the lifetime
of the process. They are never written to disk, logged, or placed on a child
process's command line; spawned runs receive them through their environment.

Mutating requests require the per-server CSRF token in the
`x-bookforge-csrf` header, and Host-header middleware rejects requests that do
not name the bound loopback port. Request bodies are capped at 64 MiB. The
dashboard page carries a content security policy, `X-Frame-Options: DENY`,
`X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, and
`Cache-Control: no-store`.

`watch` and `serve` are default-on build features (`tui` / `serve`); build with
`--no-default-features` for a minimal binary without them. Such a build has no
`serve` subcommand, and running `bookforge` without a command prints help
instead of starting the dashboard.

## A complete translation workflow

Inspect the source before spending provider tokens. `plan` is advisory only: it
reads the EPUB, constructs no provider, makes no network request, creates no
`.bookforge/` state, and neither starts nor changes a translation.

```bash
bookforge plan book.epub \
  --source English \
  --target Italian \
  --provider openrouter \
  --model openai/gpt-5.6-luna
```

Every recommendation includes its reason and a disposition: set explicitly,
keep the current `v1-fast` default, or omit an optional setting. The plan reports
dominant script; median, p90, and maximum block and scheduler-segment sizes; the
estimated default-batch output tail; provider output and thinking controls; and
the translate flags that follow from those findings. `--source` is recorded for
the operator but does not affect sizing: script is measured from the EPUB text.

Use `--json` for schema-versioned, stable output suitable for tooling:

```bash
bookforge plan book.epub --target Italian --provider deepseek --json
```

The first planning slice deliberately does not reuse prior runs. The current
job-store open path may migrate or otherwise write the database, which would
violate `plan`'s read-only contract; the output records that no prior-run
evidence was applied. Concurrency therefore stays at the `v1-fast` default with
adaptive concurrency enabled until an actual run supplies latency or 429
evidence. Glossary injection is off by default because its measured A/B found no
detectable quality effect.

Planning remains opt-in for translation. Add `--plan` to inspect the EPUB
offline, apply the actionable recommendations, and then start the job:

```bash
bookforge translate book.epub \
  --plan \
  --source Chinese \
  --target Italian \
  --provider deepseek
```

The precedence is direct setting flag, then plan, then the resolved profile,
target-policy, and provider-preset defaults. For example, an explicit
`--batch-max-items 64` is never replaced; the plan may still fill an unset
`--batch-target-tokens`. The current application surface covers batch target
tokens, batch max items, the optional batch output bound, the provider output
budget, and supported thinking suppression. Recommendations that merely keep a
default, such as offline concurrency, are not rewritten.

The EPUB archive is parsed once. Planning inspects that in-memory book and
constructs the default scheduler segments needed for its size distribution;
translation then constructs its final segments from the applied settings. Each
applied setting, typed value, and planner reason is stored in the run snapshot
under `finalize.applied_plan`. Without `--plan`, no planning inspection runs and
the existing resolved settings and translation path are unchanged.

`resume` does not accept or rerun the plan. It uses the settings captured when
the job was created, so a planner rule change after a software update cannot
silently change only the unfinished part of a book. The current cache namespace
does not include batch target/items, output budgets, or thinking suppression,
so those knobs are cache-safe; `reconfigure` may supersede its supported batch
and scheduler settings for remaining work, and its durable merged settings
compose with (without erasing) the original plan rationale.

Making planning default-on requires broader evidence: a versioned rule set
validated across substantially more books, scripts, providers, and models, with
repeatable gains in blocks recovered, failed requests, or cost per 1,000 source
characters and no material regression on existing runs.

The coverage report from `inspect` calls out visible text that BookForge would
not send for translation.

```bash
bookforge inspect book.epub
```

Test storage and provider access, then estimate the run:

```bash
bookforge doctor --storage
bookforge doctor --provider openrouter --model google/gemini-2.5-flash-lite
bookforge estimate book.epub \
  --source English \
  --target Italian \
  --provider openrouter \
  --model google/gemini-2.5-flash-lite
```

The printed estimate prices the primary translation pass only. QA review,
double-check, and repair re-runs are real extra passes during a run, so
`--pass-costs` appends an approximate breakdown for them:

```text
Pass-cost estimates (approximate planning heuristics; not metered):
  assumptions: 1 qa pass(es) @1.25x in/0.20x out | 0 double-check pass(es) @1.50x in/0.15x out | repair share 0.05
  qa review              ~120998 in / ~22264 out tokens (~$0.023174)
  repair re-runs         ~4840 in / ~5566 out tokens (~$0.002236)
  Estimated cost incl. passes: $0.025410
```

Each row reuses the same estimator and pricing catalog as the primary line:
input tokens are the primary estimate scaled by the pass's input multiplier,
and output tokens by its output multiplier, with one QA pass per run and one
double-check pass when the profile's double-check mode is enabled
(`--double-check-passes` overrides it; `--repair-share`, default `0.05`,
models the fraction of segments assumed to need a batch repair re-run). Every
figure is deterministic but approximate: cache discounts, failed-request
billing, provider rounding, and per-segment variability are not modeled, so
treat totals as planning heuristics rather than expected spend.

Start the translation. Presets bundle a provider, model, endpoint, and suitable
defaults; explicit provider flags remain available when you need them.

```bash
bookforge translate book.epub \
  --source English \
  --target Italian \
  --provider-preset open-router-paid-fast \
  --concurrency 4 \
  --qa suspicious \
  --validate-output \
  --out book.it.epub
```

`--batch-target-tokens` bounds estimated request size. An explicit
`--batch-max-output-tokens` also bounds the estimated JSON response while
packing batches, in addition to capping each provider response. The response
bound is opt-in: leaving the flag unset preserves the profile's normal batch
packing and avoids paying prompt overhead for extra requests. If a response
body still fails to decode after a retry, BookForge bisects a multi-item batch;
a single item remains the recovery floor.

`--no-thinking` asks the selected endpoint to suppress reasoning. BookForge
sends OpenRouter's `reasoning.enabled=false`, OpenAI Chat Completions'
`reasoning_effort=none`, or DeepSeek's `thinking.type=disabled`, selected from
the base URL or a known OpenRouter/DeepSeek preset credential identity. Other
OpenAI-compatible endpoints receive no guessed suppression field and produce a
warning. This includes the bundled local Ollama and llama.cpp endpoints until
they expose a compatible, documented control.

Provider-reported `completion_tokens` is the billable output aggregate and
already includes any `completion_tokens_details.reasoning_tokens` breakdown.
BookForge stores that aggregate as `tokens_output` and uses it for status and
cost reporting. It does not add the reasoning breakdown a second time.

`--qa` controls the optional LLM review pass. `off` skips it, `all` reviews
every non-empty successful, cached, or `needs_review` translation, and
`suspicious` limits those reviewable translations to segments with at least
one concrete risk signal:

- the deterministic translation pass left the segment in `needs_review`;
- the translation/source character ratio is outside `0.75..=1.5`;
- translation used the run-preserving template;
- the source carries at least four protected spans; or
- inline marker IDs, shapes, block placement, or syntax changed.

The signals are additive. Routine successful prose near a 1:1 character ratio,
using the normal template and without marker changes, is not selected. In one
32-segment prose book, deterministic validation selected 8 segments (25%);
the other signals are expected to add few segments on an ordinary book but can
matter more for markup-heavy material. QA cost scales roughly with the selected
text, so use that fraction to size the review pass against an `all` estimate.
Failed, queued, and retry-pending segments are not sent to the reviewer, and an
empty translation is never submitted even if its status is otherwise eligible.

`--qa-max-output-tokens <TOKENS>` caps each QA response and defaults to `8192`.
The value must be at least `1`. If a batched response reaches that limit or ends
as incomplete JSON, BookForge retries by bisecting the batch. One segment is the
floor: if that request is still truncated, QA records a `qa_request_failed`
warning for the segment instead of splitting again.

LLM issues are also persisted in the job's `qa_findings` data. Their kinds use
the reserved `llm_` prefix so `status` and the review page keep model judgments
distinct from deterministic validator failures. The reviewer is instructed to
report only `medium` and `high` issues and omit anything matching the `low`
boundary. `low` remains accepted in model responses for compatibility. LLM
severity maps as follows: `high` becomes a stored `error`; `medium` and `low`
become `warning`.

Repeated LLM issues are reported and stored once when both their normalized
kind and normalized source excerpt match. Source-excerpt normalization ignores
case and whitespace differences. The collapsed message retains the occurrence
count, every affected segment ID, and the available source and translation
excerpts. Issues without a source excerpt are not merged.

The command prints a job ID. Keep it: every lifecycle, monitoring, review, and
recovery command uses that stable ID.

```bash
bookforge status <job-id>
bookforge watch <job-id>
bookforge tail <job-id> --last 40
```

`watch` follows a job's durable event log live in the terminal dashboard. Its
`--refresh-ms` flag (default 150) sets the replay/refresh cadence and is clamped
to the shared 20–5,000 ms range, exactly like `serve --refresh-ms`.

After translation, review and validate the output:

```bash
bookforge review <job-id> --open
bookforge validate book.it.epub --report book.it.validation.json
```

The review HTML and JSON include the book's full source and translated text.
Treat `.bookforge/runs/<job-id>/review/` as private data.

## Job lifecycle and recovery

Pause and stop are cooperative. An in-flight provider request or finalization
step may finish before the worker reaches a safe boundary.

```bash
bookforge pause <job-id>
bookforge resume <job-id>

bookforge stop <job-id>
bookforge resume <job-id>
```

`pause` parks the current worker. `stop` asks it to exit while preserving all
completed checkpoints. `resume` first tries to wake a live worker and otherwise
starts one replacement worker. Avoid `resume --force` unless a paused worker is
known to be gone; forcing a second live worker can duplicate requests.

`retry` changes segment state but does not itself run the provider. Mark the
desired scope and then resume:

```bash
bookforge retry <job-id> --only failed
bookforge retry <job-id> --only needs-review
bookforge resume <job-id>
```

Completed compatible segments are loaded from checkpoints or cache rather than
translated again. The input snapshot and run configuration recorded with the
job protect resume behavior from later changes to the original input.

For the persistence model and state transitions, see
[CHECKPOINTING.md](CHECKPOINTING.md).

## Change a run safely

`reconfigure` applies only to work that has not crossed the relevant boundary.
It never mutates an in-flight request or retranslates completed segments.

```bash
bookforge reconfigure <job-id> \
  --concurrency 2 \
  --batch-target-tokens 2400 \
  --provider-max-attempts 4 \
  --qa suspicious
```

Mutable settings include concurrency, batch item/token budgets, provider
attempts, adaptive concurrency and batch sizing, QA, double-check mode, and
output validation. Provider, model, language, prompt, context, glossary, style,
entity, profile, and segmentation changes are rejected because they would make
existing checkpoints incompatible. Start a new job for those changes.

The command records a revisioned override file under the job's run directory.
`status` shows active overrides and the worker reports when it applies them.

### Tri-state boolean flags

Boolean *override* flags such as `--adaptive-concurrency`,
`--adaptive-batch-sizing`, `--validate-output` (reconfigure), and
`--adaptive-concurrency`, `--compact-prompts`, `--retry-failed-only`
(translate) are tri-state: unset, explicitly on, or explicitly off. They
accept both forms everywhere — a bare flag means `true`, and an explicit value
works in either spelling:

```bash
bookforge translate book.epub --target Italian --adaptive-concurrency=false
bookforge reconfigure <job-id> --validate-output        # same as =true
```

The bare form is equivalent to passing `true`; passing the flag with no value
never leaves it "unset".

## Review, flag, and correct

The static review page can export `flags.json`. Importing the file persists the
flags; wrong-translation flags mark segments as needing review, while accepted
name guidance can seed the glossary.

```bash
bookforge review <job-id> --open
bookforge ingest-flags <job-id> --flags flags.json
bookforge retry <job-id> --only needs-review
bookforge resume <job-id>
```

For a one-block segment, apply an exact human correction directly:

```bash
bookforge correct <job-id> --segment <segment-id> --text "Corrected translation"
```

Use a file for long text. A multi-block segment requires JSON containing every
block in that segment:

```json
{
  "blocks": [
    {"block_id": "block-1", "text": "First corrected block."},
    {"block_id": "block-2", "text": "Second corrected block."}
  ]
}
```

```bash
bookforge correct <job-id> --segment <segment-id> --from-file correction.json
```

BookForge validates marker constraints, stages and validates a rebuilt EPUB,
then persists the correction and atomically replaces the output. Saved human
corrections are protected from later cache, QA, and model overwrites.

## Bilingual output

Translation and output layout are separate. `replace` writes only the target
text; the append modes retain the source and add the target text.

```bash
bookforge translate book.epub \
  --target Italian \
  --provider-preset open-router-paid-fast \
  --mode append-block \
  --bilingual-style minimal \
  --out book.bilingual.epub
```

Available modes are `replace`, `append-text`, and `append-block`. Because mode
is a rebuild concern, compatible translations can be reused when you switch
between them.

## PDF ingestion and EPUB reflow

PDF conversion requires Poppler. Check the dependency before a large run:

```bash
bookforge doctor --pdf
bookforge convert paper.pdf --out paper.epub
bookforge inspect paper.epub
```

Review the conversion report before translating. It records text coverage,
layout decisions, preserved media, and low-confidence pages. See
[EPUB_PIPELINE.md](EPUB_PIPELINE.md) for pipeline details.

PDF conversion keeps the historical single-spine-section output unless an
explicit chapter prefix is supplied:

```bash
bookforge convert book.pdf --out book.epub --chapter-prefix "CHAPTER "
```

`--chapter-prefix <TEXT>` starts a new EPUB chapter before each reconstructed
text-bearing block whose whitespace-normalized visible text begins with the
literal prefix, compared case-insensitively. It checks complete block text, not
raw Poppler lines or a fixed character window, so a chapter label reconstructed
as a paragraph can still form a boundary. Content before the first match is
retained as its own front-matter chapter. No matches preserve the legacy
single-chapter EPUB byte for byte.

To prevent a broad prefix from producing thousands of tiny spine items, the
split falls back to the legacy single chapter when it finds more than 256
matches or fewer than two text blocks per match on average. The conversion
report records a warning when that guard fires.

A literal prefix cannot express headings that vary in their leading text, which
is a real limit for some languages: Chinese chapter labels such as 第一章 and
第二章 share only 第, and 第 alone also begins ordinary words, so it over-matches
into the guard rather than splitting usefully. A regular-expression form would
cover those cases, but `regex` is currently a dev-dependency only, so making it
a runtime dependency would add roughly a megabyte to the shipped single binary.
The prefix is the cheap form that covers Latin-script books; the trade is
recorded here so it can be revisited deliberately.

`reflow` is an explicit preprocessing command for broken paragraph flow; normal
EPUB reading does not silently rewrite the source.

```bash
bookforge reflow source.epub --output source.reflowed.epub --dry-run
bookforge reflow source.epub --output source.reflowed.epub
```

Use `--aggressive` only after reviewing the dry-run report. Use `--pdf-cleanup`
for EPUBs produced from PDF HTML where page furniture remains.

## Glossaries, styles, and entities

These three stores provide reusable guidance at global, series, or book scope:

- `glossary` maps source terms to required target terms.
- `style` describes register, voice, and unwanted tendencies.
- `entities` records names, target forms, gender, and narrative roles.

Inspect nested command help before editing a store:

```bash
bookforge glossary --help
bookforge style --help
bookforge entities --help
```

For a book-specific glossary, extract source candidates, ask an explicitly
chosen review model for target renderings, then accept or edit them:

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

# Non-interactive: explicitly accept every usable proposal.
bookforge glossary accept-candidates cyberiad --language "English->Italian"

# Interactive: inspect, edit, or reject individual rows.
bookforge glossary review-candidates cyberiad --language "English->Italian"
```

`extract-candidates` chooses between two extraction strategies by measuring the
scripts in the source text itself, not by looking up the language name. If most
alphabetic characters have Unicode case, the source uses the measured
capitalization heuristic. This includes German: capitalized nouns are useful
terminology candidates even though case does not distinguish them from proper
nouns. The heuristic counts a capitalized word only when it is capitalized for
some reason other than where it sits. English capitalises the first word of
every sentence, quotation, and parenthetical, and headings are title-cased by
convention, so a word attested *only* in those positions is grammar rather than
terminology — `Finally`, `Meanwhile`, `Oh`, `Yes` and `Thus` all reached the
glossary that way before this rule existed. A multi-word phrase gets a second
chance: `Ivan Ilych` may open every sentence it appears in, yet both its words
are attested mid-sentence, which is what distinguishes it from `Finally
Klapaucius`. Contractions inherit their stem, so `I'll` and `It's` are filtered
as the pronouns they are built from while `Trurl's` survives.

Predominantly caseless sources — Han, Kana, Hangul, Thai, Arabic, Hebrew, and
Devanagari — use an orthography-neutral recurrence sweep instead. Counting the
dominant character kind keeps an occasional Latin word from rerouting such a
book. The sweep considers words without relying on case and uses blocks as the
documents in an in-book TF-IDF ranking: repetition raises a token's score,
while vocabulary concentrated in a few blocks outranks function words spread
throughout the book. No language stoplist or sentence-capitalization rule is
used. Repeated headings cannot qualify without prose attestation. Unknown
language labels need no default because routing is script-derived; if the text
has no alphabetic evidence or the measurement ties, recurrence preserves recall
without assuming case. Phrases that the author marks with both italics and
enclosing quotation marks remain an explicit signal in either strategy and
bypass the frequency floor.

`--min-count` defaults to 3. On the recurrence path it is applied before the
TF-IDF ranking, and the automatic sweep admits at most the square root of the
book's word-token count (rounded up), plus explicitly author-marked phrases.
This keeps the default proposal batch sublinear in book length while making
lowercase terminology reachable. `--limit` can impose a tighter cap on the
frequency-ordered result. Each additional candidate reserves 320 output tokens
within bounded proposal requests by default, so lowering `--min-count` or
widening `--limit` is not free. The proposal model remains responsible for
rejecting ordinary language as `not_terminology`.

On The Cyberiad at `--limit 60`, the rule dropped eleven non-terms and freed
those slots for nine genuine coinages the noise had been crowding out
(`Altruizine`, `Alacritus`, `Atrocitus`, `Gargantius`, `Gozmos`, `Multitudians`,
`Mygrayn`, `Ramolda`, `Perfect Adviser`). Words the author genuinely capitalises
mid-sentence — honorifics such as `Highest` and `Most`, or `Nothing` as the name
of a machine's product — still come through, which is correct.

`propose` is a standalone, opt-in pass; `translate` never runs it implicitly.
The EPUB is required because each term is sent with one bounded source excerpt
(up to 320 characters by default, configurable with `--context-chars`).
`--qa-base-url` and `--qa-api-key-env` provide the same endpoint/key overrides
as the QA provider options. `--qa-model` is required so an inexpensive
translation model is not selected silently for this judgment-heavy pass.

`propose` sends bounded chunks rather than one book-sized request.
`--qa-max-output-tokens` is the cap for each request and defaults to 8192.
Chunks reserve 320 output tokens per candidate, so the default carries at most
25 candidates; a measured 40-candidate run on Kimi K3 spent 8277 output tokens,
about 207 each including reasoning. Passing the flag changes both the
per-request cap and the derived chunk size.

If a response reaches the cap, is incomplete JSON, or fails response-shape
validation, BookForge retries by bisecting that chunk, following the QA batch
recovery strategy. One candidate is the floor. A terminal failure remains
pending while completed chunks are persisted. The command prints an
`INCOMPLETE` summary with completed and failed candidate counts and exits with
an error, so a partially completed pass cannot be mistaken for success.

The model chooses `preserve`, `translate`, `calque`, `recreate`, `decline`, or
`not_terminology` and gives a one-sentence reason. `decline` means the item is
genuine terminology but the excerpt is insufficient for a defensible
rendering; `not_terminology` means it is ordinary language or extraction noise.
Both are targetless, but they are not interchangeable.

A rendering fills `target_text` but remains `auto_candidate`; it is inactive
until a human explicitly accepts it. For a script or batch experiment, run
`accept-candidates BOOK_ID`; for row-by-row review, use `accept N`, edit with
`set N "..."`, or run the REPL's `accept-all`. Both bulk surfaces only promote
candidates with a non-empty rendering and print stable outcome counts:

```text
Bulk acceptance: accepted=37 skipped-empty=1 skipped-model-rejected=2.
```

Model rejections are always skipped by bulk acceptance, even if a malformed or
future row happens to contain a rendering. A broad opt-in would make it too easy
to erase the distinction between "the model proposed this rendering" and "the
model said this is not terminology." Override one deliberately in
`review-candidates` instead. A decline writes no target and remains pending for
manual treatment.

A model rejection stays visible in `review-candidates` as an inactive
`auto_candidate` with a `model rejection (not terminology): ...` note. That note
both preserves the model's reason and prevents automatic re-proposal. A reviewer
can override it with `accept N` or `set N "..."`; a human `reject N` instead
changes the status to `rejected`, so machine and human decisions remain
distinguishable. Existing renderings, model rejections, and `user_seeded`,
`accepted`, or human-`rejected` decisions are not sent again. Bulk acceptance
also leaves all three settled statuses untouched.

`accept-candidates` intentionally has no category or `source_count` filters.
`extract-candidates --min-count/--limit` already controls the batch, while an
acceptance filter would make partially activated proposal sets easier to mistake
for complete experiments. Add filters only when a measured workflow needs them.

The command reports the request count plus aggregate estimated and
provider-reported token counts. As an
illustrative typical case, 50 candidates with short excerpts are about 5,000
input and 1,500 output tokens. At $3/M input and $15/M output that is roughly
$0.04 for the book; substitute the selected model's current prices.

You can also pass TOML files directly to `translate` with repeatable
`--glossary`, `--style`, and `--entities` flags. Stored scoped guidance is
selected with `--book-id` and `--series-id`. Files and stored guidance are
fingerprinted into the job configuration so incompatible cache entries are not
reused.

## Validation, diagnostics, and machine-readable progress

BookForge always applies its deterministic translation validators. `validate`
checks a completed EPUB and uses EPUBCheck when it can find it:

```bash
bookforge validate output.epub
bookforge validate output.epub --strict-epubcheck
```

Set `BOOKFORGE_EPUBCHECK` to an EPUBCheck executable, its directory, or an
`epubcheck.jar` when automatic discovery is insufficient. Without EPUBCheck,
the report records `unavailable`; BookForge's own checks still run.

## UI modes (`--ui`)

Translation-family commands (`translate`, `resume`, and the audiobook command)
accept `--ui` to select how progress is presented:

| Mode | Behavior |
| --- | --- |
| `auto` | Default. Live progress bars when stderr is attached to a terminal; silent otherwise. |
| `progress` | Always render live multi-bar progress on the terminal, even without a TTY. |
| `quiet` | No progress rendering at all — no bars, no event lines. Command-level errors still reach stderr through the normal error path, and run artifacts/checkpoints are unaffected. |
| `json` | One JSON line per progress event on stdout, wrapped in the versioned envelope `{"v":2,"kind":"event"|"audiobook","payload":{…}}` (UI-23): translation runs carry the [events.md](events.md) objects in `payload`, audiobook runs keep their own payload with its inner `"event":"audiobook_*"` discriminator. See [events.md § Stdout JSON envelope](events.md#stdout-json-envelope---ui-json). Script against stdout only after verifying you use this mode: everything human-facing moves out of the way. |
| `json-v1` | Deprecated compatibility alias. Reproduces the pre-envelope raw-line stdout dialects byte-for-byte: raw `ProgressEvent` objects for `translate`/`resume`, raw `{"event":…}` objects for `audiobook`. Use only to keep consumers pinned to v1; new automation should read the `v`/`kind` fields of `json`. |
| `tui` | Full-screen terminal dashboard attached to the in-process run (requires the `tui` build feature). |

JSON stdout purity: with `--ui json` (or its legacy alias), `translate` and
`resume` emit *only* enveloped progress lines on stdout — banners, warnings,
QA/double-check notices, and resume hints are suppressed there (they would
otherwise pollute a machine-parsed stream) instead of being redirected.
Human-readable diagnostics continue on stderr, so automation can safely parse
stdout alone. The audiobook command uses the same purity discipline with
`kind:"audiobook"` payloads (see [events.md](events.md)).

Regardless of mode, passing `--progress-jsonl <path>` durably records the full
event stream in every mode — including `quiet` and `progress` — using lazy file
creation after the job id is known.

The envelope applies **only** to stdout in these modes: the persisted
`--progress-jsonl` file log keeps the plain un-enveloped `ProgressEvent`
schema, and `tail <job-id> --json` passes those file-log lines through raw.

For automation, select JSON progress and a durable event file:

```bash
bookforge translate book.epub \
  --target Italian \
  --provider mock \
  --model mock-prefix-target \
  --ui json \
  --progress-jsonl .bookforge/runs/example/events.jsonl
```

Example stdout lines:

```jsonl
{"v":2,"kind":"event","payload":{"JobCreated":{"job_id":"job_abc","input_path":"book.epub","output_path":"book.it.epub","timestamp_ms":1710000000000}}}
{"v":2,"kind":"event","payload":{"TranslationFinished":{"succeeded":42,"cached":0,"needs_review":0,"failed":0,"input_tokens":1234,"output_tokens":456,"elapsed_ms":120000,"timestamp_ms":1710000120000}}}
```

Use `tail <job-id> --json` for persisted event objects after launch. See
[events.md](events.md) for the event schema, folding rules, and the full
envelope contract including the stream coverage table.

## Exit codes

Automation can rely on a small, stable exit-code taxonomy (also summarized by
`bookforge --help`):

| Code | Meaning |
| --- | --- |
| 0 | Success, including an intentional stop (`pause`, `stop`, TUI quit after the run finished). |
| 1 | Runtime failure: provider/config errors, IO errors, failed `doctor` checks, incomplete audiobook deliverables. |
| 2 | Usage error from the argument parser. |
| 3 | The job reached its end but segments remain failed and/or needs-review; artifacts exist but the output is not clean. Retry or resume to finish them. |
| 130 | Interrupted by Ctrl+C (or an attached `--ui tui` quit). Progress is checkpointed; run `resume` to continue. |

An interruption always reports 130 even if a provider error followed, and any
returned runtime error reports 1. `doctor` participates directly: with failed
checks it exits 1 and tells you so on stderr; scripts that relied on the old
always-green behavior can pass `--no-fail` to keep a zero exit while the report
itself still records every failure.

## Benchmark provider latency and throughput

`benchmark` sends a fixed synthetic translation request to a real provider. It
makes one provider call for each sample, so the default `--samples 5` makes five
potentially billable calls. `--tokens` defaults to 1,000 and sets each request's
maximum output tokens; it is a response-length cap, not a total token budget.

```bash
bookforge benchmark \
  --base-url https://openrouter.ai/api/v1 \
  --api-key-env OPENROUTER_API_KEY \
  --model openrouter/auto \
  --samples 5 \
  --tokens 1000
```

The command accepts the shared `--provider`, `--model`, `--base-url`,
`--api-key-env`, and `--timeout-seconds` flags. Its implementation does not,
however, read the `--provider` name: even though that parsed value defaults to
`deepseek`, it does not select a preset. Without explicit overrides,
`benchmark` constructs an OpenAI-compatible client for
`https://openrouter.ai/api/v1`, reads `OPENROUTER_API_KEY`, uses
`openrouter/auto`, and applies a 120-second timeout.

Samples run with bounded parallelism: `--concurrency` (default 1) sets how many
requests are in flight at once; `1` reproduces the old sequential behavior.

Each sample prints success or failure. Successful lines include latency, token
counts, and approximate output tokens per second. The results block always
reports success, failure, HTTP 429, and timeout counts; when requests succeed,
it also reports p50, p95, and average latency and average output throughput.
With at least one success, the recommendation classifies a rate-limited or very
slow result as `free-tier`, a fast result without rate limits as `fastest`, and
the remainder as `balanced`; it prints the selected profile name with suggested
concurrency and timeout values.

See [benchmarks.md](benchmarks.md) for the wider set of metrics to retain when
recording provider and end-to-end benchmarks.

## Audiobooks

Audiobook generation is independent of translation; its input can be a source
or translated EPUB. A dry run shows chapter and chunk counts without calling a
provider:

```bash
bookforge audiobook book.epub --provider mock --dry-run
bookforge audiobook book.epub --voice alloy --format mp3 --stitch
```

Cost estimates come from the bundled schema-1
`crates/bookforge-core/pricing/audio-providers.json`; set
`BOOKFORGE_AUDIO_PRICING_PATH` to a JSON file with the same structure to
override it for every estimate, dry run, and plan. Estimates are planning
figures only.

See [audiobooks.md](audiobooks.md) for providers, formats, resume hashing,
pruning, ffmpeg stitching, and M4B assembly. The M4B embeds the EPUB 3
`cover-image` resource when ffmpeg accepts it, with conservative legacy
cover-name fallbacks; missing or unusable artwork is non-fatal. Stitched
chapter names are stable lowercase ASCII slugs, with a deterministic hashed
fallback when stripping non-ASCII text would leave no useful name.

