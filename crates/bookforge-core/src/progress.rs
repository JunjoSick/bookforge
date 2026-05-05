pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

use serde::Serialize;

pub trait ProgressSink: Send + Sync + 'static {
    fn emit(&self, event: ProgressEvent);
}

pub struct NullProgressSink;

impl ProgressSink for NullProgressSink {
    fn emit(&self, _event: ProgressEvent) {}
}

#[derive(Debug, Clone, Serialize)]
pub enum ProgressEvent {
    JobCreated {
        job_id: String,
        input_path: String,
        output_path: String,
        timestamp_ms: u64,
    },
    StageStarted {
        stage: String,
        timestamp_ms: u64,
    },
    StageFinished {
        stage: String,
        timestamp_ms: u64,
    },
    RuntimeConfigResolved {
        profile: String,
        provider_preset: Option<String>,
        provider: String,
        model: String,
        concurrency: usize,
        max_attempts: usize,
        provider_max_attempts: usize,
        validation_max_attempts: usize,
        retry_after_policy: String,
        max_backoff_seconds: u64,
        timeout_seconds: u64,
        batch_enabled: bool,
        batch_target_tokens: usize,
        batch_max_items: usize,
        adaptive_batch_sizing: bool,
        adaptive_concurrency: bool,
        compact_prompts: bool,
        thinking_disabled: bool,
        json_mode: String,
        model_context_tokens: Option<u32>,
        max_output_tokens: Option<u32>,
        batch_max_output_tokens: Option<u32>,
        timestamp_ms: u64,
    },
    SegmentationFinished {
        segment_count: usize,
        timestamp_ms: u64,
    },
    CacheScanFinished {
        hits: usize,
        misses: usize,
        timestamp_ms: u64,
    },
    BatchQueued {
        batch_id: String,
        item_count: usize,
        timestamp_ms: u64,
    },
    BatchSplit {
        batch_id: String,
        left_items: usize,
        right_items: usize,
        timestamp_ms: u64,
    },
    BatchRepairStarted {
        failed_item_count: usize,
        timestamp_ms: u64,
    },
    BatchRepairFinished {
        repaired_items: usize,
        still_failed_items: usize,
        timestamp_ms: u64,
    },
    RequestStarted {
        request_id: String,
        batch_id: Option<String>,
        segment_id: Option<String>,
        provider: Option<String>,
        model: Option<String>,
        prompt_template: Option<String>,
        items: usize,
        estimated_input_tokens: usize,
        max_output_tokens: Option<u32>,
        active_requests: usize,
        target_concurrency: usize,
        timestamp_ms: u64,
    },
    RequestFinished {
        request_id: String,
        batch_id: Option<String>,
        segment_id: Option<String>,
        status: String,
        latency_ms: u64,
        status_code: Option<u16>,
        finish_reason: Option<String>,
        retry_count: usize,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        error_kind: Option<String>,
        timestamp_ms: u64,
    },
    SegmentStarted {
        segment_id: String,
        ordinal: usize,
        timestamp_ms: u64,
    },
    SegmentFinished {
        segment_id: String,
        status: String,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        timestamp_ms: u64,
    },
    CheckpointQueued {
        queued: usize,
        timestamp_ms: u64,
    },
    CheckpointFlushed {
        segment_id: Option<String>,
        flushed_count: usize,
        latency_ms: Option<u64>,
        timestamp_ms: u64,
    },
    ConcurrencyChanged {
        previous: usize,
        current: usize,
        reason: String,
        timestamp_ms: u64,
    },
    BatchSizingChanged {
        batch_id: Option<String>,
        previous_target: usize,
        new_target: usize,
        previous_max_items: usize,
        new_max_items: usize,
        reason: String,
        timestamp_ms: u64,
    },
    ArtifactWritten {
        path: String,
        timestamp_ms: u64,
    },
    Warning {
        kind: String,
        message: String,
        timestamp_ms: u64,
    },
    Error {
        kind: String,
        message: String,
        timestamp_ms: u64,
    },
    TranslationFinished {
        succeeded: usize,
        cached: usize,
        needs_review: usize,
        failed: usize,
        input_tokens: u64,
        output_tokens: u64,
        elapsed_ms: u64,
        timestamp_ms: u64,
    },
    DroppedEvents {
        count: usize,
        timestamp_ms: u64,
    },
}
