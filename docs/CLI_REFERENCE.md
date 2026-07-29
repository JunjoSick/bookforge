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
the live-update interval; values are clamped to the range from 50 through 5,000
milliseconds.

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
that relocated working directory.

The dashboard is deliberately unauthenticated and may hold provider API keys,
so it is only for the person at that machine. This is enforced at the network
boundary: `--bind` rejects every non-loopback address and directs remote users
to an SSH tunnel. Keys pasted into the dashboard remain in server memory only
for the lifetime of the process. They are never written to disk, logged, or
placed on a child process's command line; spawned runs receive them through
their environment.

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

Inspect the source before spending provider tokens. The coverage report calls
out visible text that BookForge would not send for translation.

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

bookforge glossary review-candidates cyberiad --language "English->Italian"
```

`extract-candidates` counts a capitalized word only when it is capitalized for
some reason other than where it sits. English capitalises the first word of
every sentence, quotation, and parenthetical, and headings are title-cased by
convention, so a word attested *only* in those positions is grammar rather than
terminology — `Finally`, `Meanwhile`, `Oh`, `Yes` and `Thus` all reached the
glossary that way before this rule existed. A multi-word phrase gets a second
chance: `Ivan Ilych` may open every sentence it appears in, yet both its words
are attested mid-sentence, which is what distinguishes it from
`Finally Klapaucius`. Contractions inherit their stem, so `I'll` and `It's` are
filtered as the pronouns they are built from while `Trurl's` survives. Author
italics and quoted coinages bypass all of this, since they are an explicit
signal.

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

One request carries every pending candidate, so `--qa-max-output-tokens` has no
fixed default: it is sized at 320 tokens per candidate, with a floor of 8192 and
a ceiling of 65536. A measured 40-candidate run on Kimi K3 spent 8277 output
tokens, about 207 each including reasoning. Pass the flag to override. If a
reasoning model still exhausts the budget before answering, the error says so
and names this flag rather than surfacing a bare parse failure.

The model chooses `preserve`, `translate`, `calque`, `recreate`, or `decline`
and gives a one-sentence reason. A proposal fills `target_text` but remains
`auto_candidate`; it is inactive until a human runs `accept N`, edits it with
`set N "..."`, or explicitly runs `accept-all`. `accept-all` only promotes
candidates that have a non-empty rendering. A decline writes no target, so the
candidate remains available for manual review. Existing proposals and
`user_seeded`, `accepted`, or `rejected` decisions are not sent again.

The command reports estimated and provider-reported token counts. As an
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

For automation, select JSON progress and a durable event file:

```bash
bookforge translate book.epub \
  --target Italian \
  --provider mock \
  --model mock-prefix-target \
  --ui json \
  --progress-jsonl .bookforge/runs/example/events.jsonl
```

Use `tail <job-id> --json` for persisted event objects after launch. See
[events.md](events.md) for the event schema and folding rules.

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

Samples currently run one at a time. `--concurrency` defaults to 1, but its
value appears only in the printed header and does not make requests parallel.

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

See [audiobooks.md](audiobooks.md) for providers, formats, resume hashing,
pruning, ffmpeg stitching, and M4B assembly.

