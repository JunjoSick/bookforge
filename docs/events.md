# Progress Events

BookForge writes translation progress as newline-delimited JSON in each run's
`events.jsonl`. The schema is the serde representation of
`bookforge_core::ProgressEvent`: an externally tagged enum where the object key
is the event variant name.

Example lines:

```jsonl
{"JobCreated":{"job_id":"job_abc","input_path":"book.epub","output_path":"book.it.epub","timestamp_ms":1710000000000}}
{"SegmentationFinished":{"segment_count":42,"timestamp_ms":1710000001000}}
{"JobPaused":{"job_id":"job_abc","timestamp_ms":1710000001500}}
{"RequestFinished":{"request_id":"req_1","batch_id":null,"segment_id":"s1","status":"ok","latency_ms":812,"status_code":200,"finish_reason":null,"retry_count":0,"input_tokens":1234,"output_tokens":456,"error_kind":null,"timestamp_ms":1710000002000}}
{"SegmentFinished":{"segment_id":"s1","status":"succeeded","input_tokens":1234,"output_tokens":456,"timestamp_ms":1710000002200}}
{"DroppedEvents":{"count":12,"timestamp_ms":1710000003000}}
{"TranslationFinished":{"succeeded":42,"cached":0,"needs_review":0,"failed":0,"input_tokens":1234,"output_tokens":456,"elapsed_ms":120000,"timestamp_ms":1710000120000}}
```

Every variant carries `timestamp_ms: u64` (Unix epoch milliseconds); it is
omitted from the per-variant fields below. Unless stated otherwise each event
is emitted by the running worker process through the same progress pipeline,
so a durable log replays identically for `watch`, `serve`, and `tail`.

## Variants

### Job lifecycle

| Variant | Fields | Semantics |
| --- | --- | --- |
| `JobCreated` | `job_id`, `input_path`, `output_path` | A new run epoch began. `resume` appends a second `JobCreated` to the same log; replay consumers treat it as an epoch boundary that resets terminal state (`finished`, pause flags). |
| `JobPaused` | `job_id` | The worker honored a pause request and parked at a safe boundary. Checkpoints written before this line are complete. |
| `JobResumed` | `job_id` | A paused live worker was woken again (wake-on-resume path; a replacement worker emits only `JobCreated`). |
| `StageStarted` / `StageFinished` | `stage` | Named stage transition (parsing, segmentation, translating, finalizing…). |
| `ArtifactWritten` | `path` | A deliverable was written (for example the output EPUB or a report). |
| `TranslationFinished` | `succeeded`, `cached`, `needs_review`, `failed`, `input_tokens`, `output_tokens`, `elapsed_ms` | Terminal summary of one run epoch. `elapsed_ms` covers this epoch, not earlier resume epochs. |

### Runtime configuration

| Variant | Fields | Semantics |
| --- | --- | --- |
| `RuntimeConfigResolved` | `profile`, `provider_preset?`, `provider`, `model`, `concurrency`, `max_attempts`, `provider_max_attempts`, `validation_max_attempts`, `retry_after_policy`, `max_backoff_seconds`, `timeout_seconds`, `batch_enabled`, `batch_target_tokens`, `batch_max_items`, `adaptive_batch_sizing`, `adaptive_concurrency`, `compact_prompts`, `thinking_disabled`, `json_mode`, `model_context_tokens?`, `max_output_tokens?`, `batch_max_output_tokens?` | The effective configuration after presets/plan/profile resolution. Enum-valued knobs serialize as their variant name strings (e.g. `json_mode: "Auto"`). Emitted once at worker start. |
| `RuntimeConfigChanged` | `revision`, `changed_fields` (array), `application` (array) | A `reconfigure` override was accepted; `application` names the boundary kinds it takes effect at (request/batch/finalize). |
| `RuntimeConfigRejected` | `revision?`, `message` | A `reconfigure` attempt violated cache-safety rules and was refused; nothing changed. |

### Planning

| Variant | Fields | Semantics |
| --- | --- | --- |
| `SegmentationFinished` | `segment_count` | Scheduler segmentation produced this many segments. Also seeds `total_segments` in folded state. |
| `CacheScanFinished` | `hits`, `misses` | Prior-run cache matching completed before translation started. |

### Batching and requests

| Variant | Fields | Semantics |
| --- | --- | --- |
| `BatchQueued` | `batch_id`, `item_count` | A batch entered the scheduler queue. |
| `BatchSplit` | `batch_id`, `left_items`, `right_items` | An oversized/failed batch was bisected into two child batches. |
| `BatchRepairStarted` | `failed_item_count` | The repair pass over failed batch items began. |
| `BatchRepairFinished` | `repaired_items`, `still_failed_items` | Repair pass ended with these outcomes. |
| `RequestStarted` | `request_id`, `batch_id?`, `segment_id?`, `provider?`, `model?`, `prompt_template?`, `items`, `estimated_input_tokens`, `max_output_tokens?`, `active_requests`, `target_concurrency` (+ optional `runtime_config_revision`, `provider_max_attempts`, serde-defaulted so older logs parse) | One provider request left. `active_requests` is the authoritative in-flight count at emit time — replay consumers sync to it rather than incrementing. |
| `RequestFinished` | `request_id`, `batch_id?`, `segment_id?`, `status`, `latency_ms`, `status_code?`, `finish_reason?`, `retry_count`, `input_tokens?`, `output_tokens?`, `error_kind?` | One provider attempt settled. `status: "ok"` means success; other statuses carry `error_kind`. |
| `ConcurrencyChanged` | `previous`, `current`, `reason` | Adaptive concurrency retuned itself. |
| `BatchSizingChanged` | `batch_id?`, `previous_target`, `new_target`, `previous_max_items`, `new_max_items`, `reason` | Adaptive batch sizing retuned itself. |

### Segments

| Variant | Fields | Semantics |
| --- | --- | --- |
| `SegmentStarted` | `segment_id`, `ordinal` | Work on a segment began. |
| `SegmentFinished` | `segment_id`, `status`, `input_tokens?`, `output_tokens?` | Terminal segment outcome. Expected `status` values are `succeeded`, `skipped_cached`, `needs_review`, and `failed`; token figures are Optional and may be absent for statuses that never reached a provider. |

### Checkpoints

| Variant | Fields | Semantics |
| --- | --- | --- |
| `CheckpointQueued` | `queued` | Current checkpoint queue depth. |
| `CheckpointFlushed` | `segment_id?`, `flushed_count`, `latency_ms?` | The dedicated checkpoint writer persisted a command; `flushed_count` counts flushes since writer start. |

The checkpoint writer deliberately never aborts a run because one write
failed: a poisoned command (for example a foreign-key violation from a
malformed model echo) is logged as an `Error{kind:"checkpoint_write"}` event
and skipped, and later checkpoints continue normally. When the writer shuts
down after dropping one or more commands it appends exactly one
`Warning{kind:"checkpoint_dropped_commands"}` whose message totals the dropped
commands alongside the successfully persisted count, so lost work is always
visible instead of silently diverging from the dashboards.

### Issues, drops, and known warning/error kinds

| Variant | Fields | Semantics |
| --- | --- | --- |
| `Warning` | `kind`, `message` | Non-fatal condition worth surfacing in dashboards/reports. |
| `Error` | `kind`, `message` | Operation-level error the run absorbed without aborting. |
| `DroppedEvents` | `count` | Honest loss accounting (see below). |

Warning and Error payloads are open-ended string pairs (`kind` + human-readable
`message`); new `kind`s may appear in minor releases. Notable kinds currently
emitted:

- `checkpoint_write` / `checkpoint_dropped_commands` — per-command checkpoint
  write failure and the writer's final dropped-vs-persisted tally (above).
- `batch_unknown_segment_failure_reattributed` — a model echoed an item ID
  twice and the parser flagged the duplicate under the placeholder segment id
  `"unknown"`; the failure was re-pointed at the real requested segment so it
  flows through NeedsReview aggregation.
- `batch_unknown_segment_failure_dropped` — same situation, but the item ID was
  never part of this batch's request and cannot be attributed to any segment;
  the phantom failure was dropped instead of escaping into persistence.
- `batch_repair_stopped` — pause/stop landed mid-repair-phase; remaining items
  stay `needs_review` and remain resumable.
- `double_check_corrections_persisted` — the double-check pass applied model
  corrections that were persisted to the store; emitted at finalize so external
  observers get one ordered visibility point before terminal events drain.
- Repair/QA plumbing failures such as `repair_batch_failed`,
  `repair_batch_invalid_response`, `qa_request_failed`,
  `systemic_truncation`, and `retry_amplification`.

`DroppedEvents` records quantify in-process progress losses honestly. The
progress channel between workers and renderers is bounded (2048 entries); if a
burst overflows it, `ChannelProgressSink` counts the discarded events instead
of blocking the run, and every live render path surfaces newly dropped counts
as a durable `DroppedEvents` line within ~500 ms, repeating each time more
events are lost. Because the marker is appended to the replay log, SQLite and
long-lived dashboards cannot permanently disagree about how much telemetry was
lost — even though no previously persisted data is affected.

## Stability

Within v1, existing variants and required fields are not removed. New variants
and optional fields may be added in minor releases.

Note: `bookforge audiobook` uses its own event names rather than the
`ProgressEvent` variants above; see [Stdout JSON envelope](#stdout-json-envelope---ui-json) below.

## Stdout JSON envelope (`--ui json`) {#stdout-json-envelope---ui-json}

The `events.jsonl` schema above is the *file* log. The `--ui json` stdout
stream wraps every line in a single versioned envelope (UI-23), so consumers no
longer face two incompatible unversioned dialects:

```jsonl
{"v":2,"kind":"event","payload":{"JobCreated":{"job_id":"job_abc","input_path":"book.epub","output_path":"book.it.epub","timestamp_ms":1710000000000}}}
{"v":2,"kind":"event","payload":{"SegmentFinished":{"segment_id":"s1","status":"succeeded","input_tokens":1234,"output_tokens":456,"timestamp_ms":1710000002200}}}
{"v":2,"kind":"audiobook","payload":{"event":"audiobook_plan","chapters":3,"chunks":27,"characters":42000}}
{"v":2,"kind":"audiobook","payload":{"event":"audiobook_finished","status":"succeeded"}}
```

Envelope rules:

- `v`: u64 wire-dialect version, currently `2`. Bumped on any change to the
  `kind` set or payload layouts. Fail fast on an unknown `v`.
- `kind`: `"event"` — payload is exactly one `ProgressEvent`, serialized as in
  the file log (externally tagged). Emitted by `translate --ui json` and
  `resume --ui json`.
- `"audiobook"` — payload is the audiobook command's own progress object and
  keeps its inner `"event":"audiobook_*"` discriminator:
  `audiobook_planning_started` (`input`), `audiobook_plan_detected_sizes`
  (`chapters`, `chunks`, `characters`), `audiobook_plan`,
  `audiobook_chunk_finished`, `audiobook_pruned`, and `audiobook_finished`
  (plan/deliverable payloads). Inner field meanings are unchanged.
- `"serialization_error"` — `payload` is `null`; emitted instead of ever
  writing a torn or malformed line.
- Unknown `kind` values must be ignored by consumers; every line is one
  self-contained JSON object terminated by LF.
- One line per event, stdout only, in the same order as the file log. All
  human-facing stdout chatter stays suppressed in this mode (UI-22).

Exactly which streams are enveloped:

| Stream | Dialect |
| --- | --- |
| `translate --ui json` / `resume --ui json` stdout | Enveloped (`v2`, `kind:"event"`) |
| `audiobook --ui json` stdout | Enveloped (`v2`, `kind:"audiobook"`) |
| `--ui json-v1` stdout (deprecated alias) | Legacy raw lines: raw `ProgressEvent` objects for translate/resume, raw `{"event":…}` objects for audiobook — byte-compatible with pre-envelope releases. |
| `events.jsonl` file log (`--progress-jsonl` or default run dir) | **Not enveloped** — always the plain `ProgressEvent` schema above. |
| `tail <job-id> --json` | **Not enveloped** — raw pass-through of the persisted file-log lines. |
| Dashboard SSE frames | Not affected: `/jobs/<id>/events` keeps its wave-1 `state`/`done` framing. |

Versioning note: there was previously no version signal on either dialect;
the historical raw streams are retroactively designated v1 and remain
reachable via `--ui json-v1`.
