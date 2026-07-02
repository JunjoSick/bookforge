# Progress Events

BookForge writes translation progress as newline-delimited JSON in each run's
`events.jsonl`. The schema is the serde representation of
`bookforge_core::ProgressEvent`: an externally tagged enum where the object key
is the event variant name.

Example lines:

```jsonl
{"JobCreated":{"job_id":"job_abc","input_path":"book.epub","output_path":"book.it.epub","timestamp_ms":1710000000000}}
{"SegmentationFinished":{"segment_count":42,"timestamp_ms":1710000001000}}
{"RequestFinished":{"request_id":"req_1","batch_id":null,"segment_id":"s1","status":"ok","latency_ms":812,"status_code":200,"finish_reason":null,"retry_count":0,"input_tokens":1234,"output_tokens":456,"error_kind":null,"timestamp_ms":1710000002000}}
{"SegmentFinished":{"segment_id":"s1","status":"succeeded","input_tokens":1234,"output_tokens":456,"timestamp_ms":1710000002200}}
{"TranslationFinished":{"succeeded":42,"cached":0,"needs_review":0,"failed":0,"input_tokens":1234,"output_tokens":456,"elapsed_ms":120000,"timestamp_ms":1710000120000}}
```

Current variants:

- `JobCreated`
- `StageStarted`
- `StageFinished`
- `RuntimeConfigResolved`
- `SegmentationFinished`
- `CacheScanFinished`
- `BatchQueued`
- `BatchSplit`
- `BatchRepairStarted`
- `BatchRepairFinished`
- `RequestStarted`
- `RequestFinished`
- `SegmentStarted`
- `SegmentFinished`
- `CheckpointQueued`
- `CheckpointFlushed`
- `ConcurrencyChanged`
- `BatchSizingChanged`
- `ArtifactWritten`
- `Warning`
- `Error`
- `TranslationFinished`
- `DroppedEvents`

Within v1, existing variants and required fields are not removed. New variants
and optional fields may be added in minor releases.
