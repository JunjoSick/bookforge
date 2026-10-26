pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

use std::collections::{HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

pub trait ProgressSink: Send + Sync + 'static {
    fn emit(&self, event: ProgressEvent);
}

pub struct NullProgressSink;

impl ProgressSink for NullProgressSink {
    fn emit(&self, _event: ProgressEvent) {}
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ProgressEvent {
    JobCreated {
        job_id: String,
        input_path: String,
        output_path: String,
        timestamp_ms: u64,
    },
    JobPaused {
        job_id: String,
        timestamp_ms: u64,
    },
    JobResumed {
        job_id: String,
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
    RuntimeConfigChanged {
        revision: u64,
        changed_fields: Vec<String>,
        application: Vec<String>,
        timestamp_ms: u64,
    },
    RuntimeConfigRejected {
        revision: Option<u64>,
        message: String,
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
    /// Whole-job checkpoint totals at the start of a resumed run, emitted after
    /// its cache scan. Lets a fresh viewer show the job's saved progress rather
    /// than only this invocation's work. Token totals describe the saved
    /// checkpoints, not a provider billing ledger.
    ProgressRestored {
        total_segments: usize,
        succeeded: usize,
        cached: usize,
        needs_review: usize,
        failed: usize,
        input_tokens: u64,
        output_tokens: u64,
        timestamp_ms: u64,
    },
    /// Prior terminal statuses of the segments this resumed run will retry,
    /// emitted in bounded chunks, so a retried segment replaces its earlier
    /// outcome instead of being counted twice.
    SegmentStatusesRestored {
        statuses: Vec<(String, String)>,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        runtime_config_revision: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_max_attempts: Option<usize>,
        /// Per-request effective timeout (seconds) after the latency-aware
        /// output-budget extension, when the engine knows the configured
        /// timeout. `None`/absent for older rows and library callers that do
        /// not surface a timeout.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effective_timeout_seconds: Option<u64>,
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
    /// Periodic in-flight heartbeat emitted while a batch request is being
    /// served, so dashboards can show liveness between RequestStarted and
    /// RequestFinished. Informational only; consumers must fold it as a
    /// no-op and log writers must not treat it as an important (flush
    /// forcing) event.
    RequestProgress {
        batch_id: String,
        items: usize,
        elapsed_ms: u64,
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

/// Returns the `timestamp_ms` carried by any `ProgressEvent` variant.
pub fn event_timestamp_ms(event: &ProgressEvent) -> u64 {
    use ProgressEvent::*;
    match event {
        JobCreated { timestamp_ms, .. }
        | JobPaused { timestamp_ms, .. }
        | JobResumed { timestamp_ms, .. }
        | StageStarted { timestamp_ms, .. }
        | StageFinished { timestamp_ms, .. }
        | RuntimeConfigResolved { timestamp_ms, .. }
        | RuntimeConfigChanged { timestamp_ms, .. }
        | RuntimeConfigRejected { timestamp_ms, .. }
        | SegmentationFinished { timestamp_ms, .. }
        | CacheScanFinished { timestamp_ms, .. }
        | ProgressRestored { timestamp_ms, .. }
        | SegmentStatusesRestored { timestamp_ms, .. }
        | BatchQueued { timestamp_ms, .. }
        | BatchSplit { timestamp_ms, .. }
        | BatchRepairStarted { timestamp_ms, .. }
        | BatchRepairFinished { timestamp_ms, .. }
        | RequestStarted { timestamp_ms, .. }
        | RequestFinished { timestamp_ms, .. }
        | RequestProgress { timestamp_ms, .. }
        | SegmentStarted { timestamp_ms, .. }
        | SegmentFinished { timestamp_ms, .. }
        | CheckpointQueued { timestamp_ms, .. }
        | CheckpointFlushed { timestamp_ms, .. }
        | ConcurrencyChanged { timestamp_ms, .. }
        | BatchSizingChanged { timestamp_ms, .. }
        | ArtifactWritten { timestamp_ms, .. }
        | Warning { timestamp_ms, .. }
        | Error { timestamp_ms, .. }
        | TranslationFinished { timestamp_ms, .. }
        | DroppedEvents { timestamp_ms, .. } => *timestamp_ms,
    }
}

const RECENT_EVENTS_CAP: usize = 500;
const RECENT_ISSUES_CAP: usize = 200;
const FLAGGED_SEGMENTS_CAP: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IssueLevel {
    Warning,
    Error,
}

/// A warning/error surfaced during a run, retained for display panels.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueEntry {
    pub level: IssueLevel,
    pub kind: String,
    pub message: String,
    pub timestamp_ms: u64,
}

/// Renderer-agnostic, foldable view of a translation run.
///
/// `RunState` is the single source of truth for displayable numbers. It is
/// built purely by folding [`ProgressEvent`]s (live over a channel, or replayed
/// from a JSONL log), so the same state powers the indicatif bars, the ratatui
/// TUI, and a future web dashboard. Timing is derived from event
/// `timestamp_ms` (not a wall clock) so that replaying a finished log yields
/// identical numbers to watching it live.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunState {
    // Identity / configuration.
    pub job_id: Option<String>,
    pub input_path: Option<String>,
    pub output_path: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub configured_concurrency: usize,
    #[serde(default)]
    pub runtime_config_revision: u64,
    #[serde(default)]
    pub runtime_config_changed_fields: Vec<String>,
    #[serde(default)]
    pub runtime_config_rejection: Option<String>,

    // Progress.
    pub stage: Option<String>,
    pub total_segments: usize,
    pub done_segments: usize,
    pub cached: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub needs_review: usize,
    pub active_requests: usize,
    pub target_concurrency: usize,
    pub checkpoint_flushed: usize,

    /// Segments this invocation still owed provider work when it started
    /// (cache misses), and how many of them reached a terminal outcome. Cache
    /// hits, restored checkpoints and earlier runs are excluded, so rate and
    /// ETA describe fresh work only.
    #[serde(default)]
    pub run_total_segments: usize,
    #[serde(default)]
    pub run_completed_segments: usize,
    /// Latest known status per segment, so a repeated outcome replaces the
    /// earlier one instead of being counted again.
    #[serde(skip)]
    segment_statuses: HashMap<String, String>,
    #[serde(skip)]
    run_completed_ids: HashSet<String>,

    // Tokens.
    pub input_tokens: u64,
    pub output_tokens: u64,

    // Timing (epoch milliseconds, taken from event timestamps).
    pub first_timestamp_ms: Option<u64>,
    pub last_timestamp_ms: Option<u64>,

    // Terminal summary.
    pub finished: bool,
    pub finished_elapsed_ms: Option<u64>,
    pub paused: bool,

    // Bounded log surfaces for the TUI / web panels. The full ring is kept in
    // memory for in-process renderers (TUI/indicatif read it directly), but only
    // the most recent slice is serialized — the web dashboard streams this state
    // as JSON on every change, and shipping the whole ring each frame is wasteful.
    #[serde(serialize_with = "serialize_recent_tail")]
    pub recent_events: VecDeque<ProgressEvent>,
    #[serde(serialize_with = "serialize_recent_tail")]
    pub recent_issues: VecDeque<IssueEntry>,
    pub failed_segments: Vec<String>,
    pub needs_review_segments: Vec<String>,
}

impl RunState {
    /// Build state by folding an entire sequence of events (e.g. a JSONL replay).
    pub fn from_events(events: impl IntoIterator<Item = ProgressEvent>) -> Self {
        let mut state = Self::default();
        for event in events {
            state.fold(&event);
        }
        state
    }

    /// Apply a single event to the state. Pure data folding — no rendering.
    pub fn fold(&mut self, event: &ProgressEvent) {
        let ts = event_timestamp_ms(event);
        if ts != 0 {
            if self.first_timestamp_ms.is_none() {
                self.first_timestamp_ms = Some(ts);
            }
            self.last_timestamp_ms = Some(ts);
        }

        match event {
            ProgressEvent::JobCreated {
                job_id,
                input_path,
                output_path,
                ..
            } => {
                self.job_id = Some(job_id.clone());
                self.input_path = Some(input_path.clone());
                self.output_path = Some(output_path.clone());
                // `resume` appends a fresh JobCreated to the existing event log.
                // A prior TranslationFinished belongs to the previous run epoch
                // and must not make live replay terminate before the resumed
                // worker emits its first new progress event.
                self.finished = false;
                self.finished_elapsed_ms = None;
                self.paused = false;
                // Each invocation is its own timing and throughput window: idle
                // time between runs and earlier work never feed rate or ETA. A
                // zero timestamp keeps the previous baseline instead of erasing it.
                if ts != 0 {
                    self.first_timestamp_ms = Some(ts);
                }
                self.active_requests = 0;
                self.checkpoint_flushed = 0;
                self.run_total_segments = self.total_segments;
                self.run_completed_segments = 0;
                self.segment_statuses.clear();
                self.run_completed_ids.clear();
            }
            ProgressEvent::JobPaused { job_id, .. } => {
                self.job_id = Some(job_id.clone());
                self.paused = true;
            }
            ProgressEvent::JobResumed { job_id, .. } => {
                self.job_id = Some(job_id.clone());
                self.paused = false;
            }
            ProgressEvent::RuntimeConfigResolved {
                provider,
                model,
                concurrency,
                ..
            } => {
                self.provider = Some(provider.clone());
                self.model = Some(model.clone());
                self.configured_concurrency = *concurrency;
                self.target_concurrency = *concurrency;
            }
            ProgressEvent::RuntimeConfigChanged {
                revision,
                changed_fields,
                ..
            } => {
                self.runtime_config_revision = *revision;
                self.runtime_config_changed_fields = changed_fields.clone();
                self.runtime_config_rejection = None;
            }
            ProgressEvent::RuntimeConfigRejected { message, .. } => {
                self.runtime_config_rejection = Some(message.clone());
            }
            ProgressEvent::StageStarted { stage, .. } => {
                self.stage = Some(stage.clone());
            }
            ProgressEvent::StageFinished { .. } => {
                // Mirrors the legacy renderer, which switches the stage label
                // to "translating" once the setup stage completes.
                self.stage = Some("translating".to_string());
            }
            ProgressEvent::SegmentationFinished { segment_count, .. } => {
                self.total_segments = *segment_count;
                self.run_total_segments = *segment_count;
            }
            ProgressEvent::CacheScanFinished { hits, misses, .. } => {
                self.cached = *hits;
                self.succeeded = 0;
                self.needs_review = 0;
                self.failed = 0;
                self.done_segments = *hits;
                self.run_total_segments = *misses;
            }
            ProgressEvent::ProgressRestored {
                total_segments,
                succeeded,
                cached,
                needs_review,
                failed,
                input_tokens,
                output_tokens,
                ..
            } => {
                self.total_segments = *total_segments;
                self.succeeded = *succeeded;
                self.cached = *cached;
                self.needs_review = *needs_review;
                self.failed = *failed;
                self.done_segments = succeeded + cached + needs_review + failed;
                // Authoritative checkpoint totals, as in TranslationFinished.
                self.input_tokens = *input_tokens;
                self.output_tokens = *output_tokens;
            }
            ProgressEvent::SegmentStatusesRestored { statuses, .. } => {
                self.segment_statuses.extend(statuses.iter().cloned());
            }
            ProgressEvent::SegmentFinished {
                segment_id,
                status,
                input_tokens,
                output_tokens,
                ..
            } => {
                // A segment that finishes again (retry, resume) replaces its
                // earlier outcome rather than adding a second one.
                if let Some(previous) = self
                    .segment_statuses
                    .insert(segment_id.clone(), status.clone())
                {
                    self.remove_terminal_status(&previous);
                }
                self.failed_segments.retain(|id| id != segment_id);
                self.needs_review_segments.retain(|id| id != segment_id);
                match status.as_str() {
                    "succeeded" => self.succeeded += 1,
                    "skipped_cached" => self.cached += 1,
                    "needs_review" => {
                        self.needs_review += 1;
                        push_capped_vec(
                            &mut self.needs_review_segments,
                            segment_id.clone(),
                            FLAGGED_SEGMENTS_CAP,
                        );
                    }
                    "failed" => {
                        self.failed += 1;
                        push_capped_vec(
                            &mut self.failed_segments,
                            segment_id.clone(),
                            FLAGGED_SEGMENTS_CAP,
                        );
                    }
                    _ => {}
                }
                self.done_segments = self.succeeded + self.cached + self.needs_review + self.failed;
                if matches!(status.as_str(), "succeeded" | "needs_review" | "failed")
                    && self.run_completed_ids.insert(segment_id.clone())
                {
                    self.run_completed_segments += 1;
                }
                // Usage is additive: a retried segment's earlier attempt was
                // still spent, so its tokens are not discarded.
                if let Some(tokens) = input_tokens {
                    self.input_tokens += *tokens;
                }
                if let Some(tokens) = output_tokens {
                    self.output_tokens += *tokens;
                }
            }
            ProgressEvent::RequestStarted {
                active_requests,
                target_concurrency,
                ..
            } => {
                // Sync to the event's authoritative in-flight count rather than
                // incrementing, so a dropped/missed event self-corrects on the
                // next start instead of leaving the gauge permanently skewed.
                self.active_requests = *active_requests;
                self.target_concurrency = *target_concurrency;
            }
            ProgressEvent::RequestFinished { .. } => {
                self.active_requests = self.active_requests.saturating_sub(1);
            }
            ProgressEvent::ConcurrencyChanged { current, .. } => {
                self.target_concurrency = *current;
            }
            ProgressEvent::CheckpointFlushed { flushed_count, .. } => {
                self.checkpoint_flushed = *flushed_count;
            }
            ProgressEvent::Warning {
                kind,
                message,
                timestamp_ms,
            } => {
                push_capped_deque(
                    &mut self.recent_issues,
                    IssueEntry {
                        level: IssueLevel::Warning,
                        kind: kind.clone(),
                        message: message.clone(),
                        timestamp_ms: *timestamp_ms,
                    },
                    RECENT_ISSUES_CAP,
                );
            }
            ProgressEvent::Error {
                kind,
                message,
                timestamp_ms,
            } => {
                push_capped_deque(
                    &mut self.recent_issues,
                    IssueEntry {
                        level: IssueLevel::Error,
                        kind: kind.clone(),
                        message: message.clone(),
                        timestamp_ms: *timestamp_ms,
                    },
                    RECENT_ISSUES_CAP,
                );
            }
            ProgressEvent::TranslationFinished {
                succeeded,
                cached,
                needs_review,
                failed,
                input_tokens,
                output_tokens,
                elapsed_ms,
                ..
            } => {
                self.succeeded = *succeeded;
                self.cached = *cached;
                self.needs_review = *needs_review;
                self.failed = *failed;
                self.input_tokens = *input_tokens;
                self.output_tokens = *output_tokens;
                self.done_segments = *succeeded + *cached + *needs_review + *failed;
                self.finished = true;
                self.finished_elapsed_ms = Some(*elapsed_ms);
                self.paused = false;
                self.active_requests = 0;
            }
            _ => {}
        }

        push_capped_deque(&mut self.recent_events, event.clone(), RECENT_EVENTS_CAP);
    }

    /// Segments not yet in a terminal state.
    pub fn remaining(&self) -> usize {
        self.total_segments.saturating_sub(self.done_segments)
    }

    fn remove_terminal_status(&mut self, status: &str) {
        let count = match status {
            "succeeded" => &mut self.succeeded,
            "skipped_cached" => &mut self.cached,
            "needs_review" => &mut self.needs_review,
            "failed" => &mut self.failed,
            _ => return,
        };
        *count = count.saturating_sub(1);
    }

    /// Current invocation's elapsed time in seconds. A finished run reports
    /// its own measured duration; otherwise the span since the invocation's
    /// first event (gaps between resumed runs are excluded).
    pub fn elapsed_secs(&self) -> f64 {
        if let Some(elapsed) = self.finished_elapsed_ms {
            return elapsed as f64 / 1000.0;
        }
        match (self.first_timestamp_ms, self.last_timestamp_ms) {
            (Some(start), Some(end)) if end >= start => (end - start) as f64 / 1000.0,
            _ => 0.0,
        }
    }

    /// Fresh terminal completions per minute in this invocation, excluding
    /// cache hits and restored checkpoints; 0 before any time has elapsed.
    pub fn segments_per_minute(&self) -> f64 {
        let secs = self.elapsed_secs();
        if secs <= 0.0 {
            return 0.0;
        }
        self.run_completed_segments as f64 / secs * 60.0
    }

    /// This invocation's provider work still outstanding.
    pub fn run_remaining(&self) -> usize {
        self.run_total_segments
            .saturating_sub(self.run_completed_segments)
    }

    /// Estimated seconds until this invocation's provider work completes, or 0
    /// when it is finished or cannot be estimated.
    pub fn eta_secs(&self) -> f64 {
        let per_min = self.segments_per_minute();
        if self.finished || per_min <= 0.0 {
            return 0.0;
        }
        self.run_remaining() as f64 / (per_min / 60.0)
    }
}

/// How many of the most recent log entries to include when a `RunState` is
/// serialized (e.g. streamed to the web dashboard). The in-memory ring is
/// larger; consumers that need full history read the deque directly.
const SERIALIZED_RECENT_TAIL: usize = 80;

/// Serialize only the last [`SERIALIZED_RECENT_TAIL`] entries of a ring buffer,
/// preserving order. Keeps SSE frames small without shrinking the in-memory ring.
fn serialize_recent_tail<S, T>(items: &VecDeque<T>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
    T: Serialize,
{
    let skip = items.len().saturating_sub(SERIALIZED_RECENT_TAIL);
    serializer.collect_seq(items.iter().skip(skip))
}

fn push_capped_vec<T>(items: &mut Vec<T>, item: T, cap: usize) {
    if items.len() >= cap {
        items.remove(0);
    }
    items.push(item);
}

fn push_capped_deque<T>(items: &mut VecDeque<T>, item: T, cap: usize) {
    while items.len() >= cap {
        items.pop_front();
    }
    items.push_back(item);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg_finished(id: &str, status: &str, ts: u64) -> ProgressEvent {
        ProgressEvent::SegmentFinished {
            segment_id: id.to_string(),
            status: status.to_string(),
            input_tokens: Some(10),
            output_tokens: Some(20),
            timestamp_ms: ts,
        }
    }

    #[test]
    fn from_events_matches_translation_finished_totals() {
        let events = vec![
            ProgressEvent::JobCreated {
                job_id: "job_1".into(),
                input_path: "in.epub".into(),
                output_path: "out.epub".into(),
                timestamp_ms: 1_000,
            },
            ProgressEvent::SegmentationFinished {
                segment_count: 5,
                timestamp_ms: 1_100,
            },
            ProgressEvent::CacheScanFinished {
                hits: 1,
                misses: 4,
                timestamp_ms: 1_200,
            },
            seg_finished("s2", "succeeded", 2_000),
            seg_finished("s3", "needs_review", 3_000),
            seg_finished("s4", "failed", 4_000),
            ProgressEvent::TranslationFinished {
                succeeded: 2,
                cached: 1,
                needs_review: 1,
                failed: 1,
                input_tokens: 123,
                output_tokens: 456,
                elapsed_ms: 3_000,
                timestamp_ms: 4_000,
            },
        ];

        let state = RunState::from_events(events);
        assert_eq!(state.total_segments, 5);
        assert_eq!(state.done_segments, 5);
        assert_eq!(state.succeeded, 2);
        assert_eq!(state.cached, 1);
        assert_eq!(state.needs_review, 1);
        assert_eq!(state.failed, 1);
        assert_eq!(state.input_tokens, 123);
        assert_eq!(state.output_tokens, 456);
        assert!(state.finished);
        assert_eq!(state.finished_elapsed_ms, Some(3_000));
        assert_eq!(state.failed_segments, vec!["s4".to_string()]);
        assert_eq!(state.needs_review_segments, vec!["s3".to_string()]);
    }

    #[test]
    fn second_job_created_clears_previous_epoch_terminal_state() {
        let first_created = ProgressEvent::JobCreated {
            job_id: "job_1".into(),
            input_path: "in.epub".into(),
            output_path: "first.epub".into(),
            timestamp_ms: 1_000,
        };
        let finished = ProgressEvent::TranslationFinished {
            succeeded: 2,
            cached: 1,
            needs_review: 0,
            failed: 0,
            input_tokens: 123,
            output_tokens: 456,
            elapsed_ms: 3_000,
            timestamp_ms: 4_000,
        };
        let resumed_created = ProgressEvent::JobCreated {
            job_id: "job_1".into(),
            input_path: "in.epub".into(),
            output_path: "resumed.epub".into(),
            timestamp_ms: 5_000,
        };

        let mut state = RunState::from_events([first_created, finished]);
        assert!(state.finished);
        assert_eq!(state.finished_elapsed_ms, Some(3_000));
        state.paused = true;

        state.fold(&resumed_created);

        assert!(!state.finished);
        assert_eq!(state.finished_elapsed_ms, None);
        assert!(!state.paused);
        assert_eq!(state.output_path.as_deref(), Some("resumed.epub"));
        assert_eq!(state.done_segments, 3);
        assert_eq!(state.input_tokens, 123);
        assert_eq!(state.output_tokens, 456);
    }

    #[test]
    fn timing_is_derived_from_event_timestamps() {
        // Done across a 60s span -> 4 segments/min, regardless of wall clock.
        let mut state = RunState::default();
        state.fold(&ProgressEvent::SegmentationFinished {
            segment_count: 8,
            timestamp_ms: 10_000,
        });
        for (i, ts) in [25_000u64, 40_000, 55_000, 70_000].into_iter().enumerate() {
            state.fold(&seg_finished(&format!("s{i}"), "succeeded", ts));
        }
        assert_eq!(state.done_segments, 4);
        assert!((state.elapsed_secs() - 60.0).abs() < 1e-9);
        assert!((state.segments_per_minute() - 4.0).abs() < 1e-9);
        // 4 remaining at 4/min -> ~60s ETA.
        assert!((state.eta_secs() - 60.0).abs() < 1e-9);
    }

    #[test]
    fn cached_segments_do_not_inflate_translation_rate() {
        let state = RunState::from_events([
            ProgressEvent::SegmentationFinished {
                segment_count: 100,
                timestamp_ms: 1_000,
            },
            ProgressEvent::CacheScanFinished {
                hits: 90,
                misses: 10,
                timestamp_ms: 1_001,
            },
            seg_finished("fresh", "succeeded", 61_000),
        ]);
        assert_eq!(state.done_segments, 91);
        assert!((state.segments_per_minute() - 1.0).abs() < 1e-9);
        // Nine fresh segments remain at one per minute.
        assert!((state.eta_secs() - 540.0).abs() < 1e-9);
    }

    #[test]
    fn restored_progress_matches_fresh_and_historical_resume_views() {
        let created = |timestamp_ms| ProgressEvent::JobCreated {
            job_id: "job_1".into(),
            input_path: "in.epub".into(),
            output_path: "out.epub".into(),
            timestamp_ms,
        };
        // A viewer that replayed the original run's log...
        let mut replay = RunState::from_events([
            created(1_000),
            ProgressEvent::SegmentationFinished {
                segment_count: 8,
                timestamp_ms: 1_001,
            },
            seg_finished("old-success", "succeeded", 10_000),
            seg_finished("retry", "failed", 20_000),
        ]);
        // ...and a viewer that only sees the resumed run, a day later.
        let resumed = vec![
            created(86_401_000),
            ProgressEvent::SegmentationFinished {
                segment_count: 8,
                timestamp_ms: 86_401_001,
            },
            ProgressEvent::CacheScanFinished {
                hits: 0,
                misses: 4,
                timestamp_ms: 86_401_002,
            },
            ProgressEvent::ProgressRestored {
                total_segments: 8,
                succeeded: 2,
                cached: 1,
                needs_review: 1,
                failed: 1,
                input_tokens: 1_000,
                output_tokens: 2_000,
                timestamp_ms: 86_401_003,
            },
            ProgressEvent::SegmentStatusesRestored {
                statuses: vec![("retry".into(), "failed".into())],
                timestamp_ms: 86_401_004,
            },
            seg_finished("retry", "succeeded", 86_461_000),
        ];
        let fresh = RunState::from_events(resumed.clone());
        for event in resumed {
            replay.fold(&event);
        }
        for state in [&fresh, &replay] {
            assert_eq!(state.done_segments, 5);
            assert_eq!(
                (
                    state.succeeded,
                    state.cached,
                    state.needs_review,
                    state.failed
                ),
                (3, 1, 1, 0)
            );
            assert_eq!((state.input_tokens, state.output_tokens), (1_010, 2_020));
            assert_eq!(state.elapsed_secs(), 60.0);
            assert_eq!(state.run_completed_segments, 1);
            assert_eq!(state.run_total_segments, 4);
            assert_eq!(state.segments_per_minute(), 1.0);
            assert_eq!(state.eta_secs(), 180.0);
            assert!(!state.failed_segments.contains(&"retry".to_string()));
            let serialized = serde_json::to_value(state).unwrap();
            assert_eq!(serialized["run_completed_segments"], 1);
            assert!(serialized.get("segment_statuses").is_none());
        }
    }

    #[test]
    fn status_replacement_keeps_progress_unique_without_discarding_usage() {
        let state = RunState::from_events([
            ProgressEvent::SegmentationFinished {
                segment_count: 2,
                timestamp_ms: 1_000,
            },
            seg_finished("s1", "failed", 31_000),
            seg_finished("s1", "succeeded", 61_000),
        ]);
        assert_eq!(state.done_segments, 1);
        assert_eq!((state.succeeded, state.failed), (1, 0));
        assert!(state.failed_segments.is_empty());
        assert_eq!(state.segments_per_minute(), 1.0);
        // Both attempts were spent, so their usage stays counted.
        assert_eq!((state.input_tokens, state.output_tokens), (20, 40));
    }

    #[test]
    fn finished_elapsed_matches_the_current_invocation_summary() {
        let mut state = RunState::from_events([seg_finished("s1", "succeeded", 1_000)]);
        state.fold(&ProgressEvent::TranslationFinished {
            succeeded: 1,
            cached: 9,
            needs_review: 0,
            failed: 0,
            input_tokens: 10,
            output_tokens: 20,
            elapsed_ms: 60_000,
            timestamp_ms: 86_461_000,
        });
        assert_eq!(state.elapsed_secs(), 60.0);
        assert_eq!(state.segments_per_minute(), 1.0);
        assert_eq!(state.eta_secs(), 0.0);
        assert_eq!(state.active_requests, 0);
    }

    #[test]
    fn restored_progress_events_round_trip_through_json() {
        for event in [
            ProgressEvent::ProgressRestored {
                total_segments: 3,
                succeeded: 1,
                cached: 1,
                needs_review: 0,
                failed: 1,
                input_tokens: 5,
                output_tokens: 7,
                timestamp_ms: 11,
            },
            ProgressEvent::SegmentStatusesRestored {
                statuses: vec![("s1".into(), "failed".into())],
                timestamp_ms: 12,
            },
        ] {
            let line = serde_json::to_string(&event).unwrap();
            let parsed: ProgressEvent = serde_json::from_str(&line).unwrap();
            assert_eq!(event_timestamp_ms(&parsed), event_timestamp_ms(&event));
        }
    }

    #[test]
    fn runtime_config_events_fold_into_replayable_state() {
        let mut state = RunState::default();
        state.fold(&ProgressEvent::RuntimeConfigRejected {
            revision: Some(2),
            message: "invalid concurrency".to_string(),
            timestamp_ms: 10,
        });
        assert_eq!(
            state.runtime_config_rejection.as_deref(),
            Some("invalid concurrency")
        );

        state.fold(&ProgressEvent::RuntimeConfigChanged {
            revision: 3,
            changed_fields: vec!["concurrency".to_string(), "batch-max-items".to_string()],
            application: vec!["next_request".to_string(), "next_batch".to_string()],
            timestamp_ms: 20,
        });

        assert_eq!(state.runtime_config_revision, 3);
        assert_eq!(
            state.runtime_config_changed_fields,
            vec!["concurrency".to_string(), "batch-max-items".to_string()]
        );
        assert!(state.runtime_config_rejection.is_none());
        let json = serde_json::to_string(&state).unwrap();
        let replayed: RunState = serde_json::from_str(&json).unwrap();
        assert_eq!(replayed.runtime_config_revision, 3);
    }

    #[test]
    fn active_requests_tracks_start_and_finish() {
        let mut state = RunState::default();
        let started = ProgressEvent::RequestStarted {
            request_id: "r1".into(),
            batch_id: None,
            segment_id: None,
            provider: None,
            model: None,
            prompt_template: None,
            items: 1,
            estimated_input_tokens: 0,
            max_output_tokens: None,
            active_requests: 1,
            target_concurrency: 4,
            runtime_config_revision: None,
            provider_max_attempts: None,
            effective_timeout_seconds: None,
            timestamp_ms: 1,
        };
        state.fold(&started);
        assert_eq!(state.active_requests, 1);
        assert_eq!(state.target_concurrency, 4);
        state.fold(&ProgressEvent::RequestFinished {
            request_id: "r1".into(),
            batch_id: None,
            segment_id: None,
            status: "ok".into(),
            latency_ms: 5,
            status_code: Some(200),
            finish_reason: None,
            retry_count: 0,
            input_tokens: None,
            output_tokens: None,
            error_kind: None,
            timestamp_ms: 2,
        });
        assert_eq!(state.active_requests, 0);
    }

    #[test]
    fn issues_are_collected_by_level() {
        let mut state = RunState::default();
        state.fold(&ProgressEvent::Warning {
            kind: "length_ratio".into(),
            message: "ratio off".into(),
            timestamp_ms: 1,
        });
        state.fold(&ProgressEvent::Error {
            kind: "provider".into(),
            message: "boom".into(),
            timestamp_ms: 2,
        });
        assert_eq!(state.recent_issues.len(), 2);
        assert_eq!(state.recent_issues[0].level, IssueLevel::Warning);
        assert_eq!(state.recent_issues[1].level, IssueLevel::Error);
    }

    #[test]
    fn events_roundtrip_through_json() {
        let event = seg_finished("s1", "succeeded", 42);
        let line = serde_json::to_string(&event).unwrap();
        let parsed: ProgressEvent = serde_json::from_str(&line).unwrap();
        let mut state = RunState::default();
        state.fold(&parsed);
        assert_eq!(state.succeeded, 1);
    }

    #[test]
    fn older_request_started_events_default_runtime_audit_fields() {
        let json = r#"{"RequestStarted":{"request_id":"r1","batch_id":null,"segment_id":null,"provider":null,"model":null,"prompt_template":null,"items":1,"estimated_input_tokens":2,"max_output_tokens":256,"active_requests":1,"target_concurrency":2,"timestamp_ms":3}}"#;
        let event: ProgressEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(
            event,
            ProgressEvent::RequestStarted {
                runtime_config_revision: None,
                provider_max_attempts: None,
                ..
            }
        ));
    }

    #[test]
    fn request_progress_round_trips_and_is_not_an_issue() {
        let event = ProgressEvent::RequestProgress {
            batch_id: "batch_0001".into(),
            items: 4,
            elapsed_ms: 15_023,
            timestamp_ms: 42,
        };
        let line = serde_json::to_string(&event).unwrap();
        let parsed: ProgressEvent = serde_json::from_str(&line).unwrap();
        assert_eq!(event_timestamp_ms(&parsed), 42);
        // Folding a heartbeat must not mutate any counter beyond the event ring.
        let mut state = RunState::default();
        state.fold(&parsed);
        assert_eq!(state.active_requests, 0);
        assert_eq!(state.recent_events.len(), 1);
        assert_eq!(state.recent_issues.len(), 0);
    }

    #[test]
    fn job_pause_events_fold_into_state() {
        let mut state = RunState::default();
        state.fold(&ProgressEvent::JobPaused {
            job_id: "job_pause".into(),
            timestamp_ms: 10,
        });
        assert_eq!(state.job_id.as_deref(), Some("job_pause"));
        assert!(state.paused);

        state.fold(&ProgressEvent::JobResumed {
            job_id: "job_pause".into(),
            timestamp_ms: 20,
        });
        assert!(!state.paused);
    }

    #[test]
    fn serialized_recent_events_are_capped_to_tail() {
        let mut state = RunState::default();
        let n = SERIALIZED_RECENT_TAIL + 40;
        for i in 0..n {
            state.fold(&seg_finished(&format!("s{i}"), "succeeded", i as u64 + 1));
        }
        // The in-memory ring keeps everything (well under RECENT_EVENTS_CAP)...
        assert_eq!(state.recent_events.len(), n);
        // ...but the serialized view (what the dashboard streams) is the tail only.
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert_eq!(
            json["recent_events"].as_array().unwrap().len(),
            SERIALIZED_RECENT_TAIL
        );
    }

    #[test]
    fn request_started_syncs_authoritative_active_count() {
        // A missed RequestFinished must not permanently skew the gauge: the next
        // RequestStarted resets it to the event's own active_requests value.
        let mut state = RunState::default();
        let started = |active: usize, ts: u64| ProgressEvent::RequestStarted {
            request_id: format!("r{ts}"),
            batch_id: None,
            segment_id: None,
            provider: None,
            model: None,
            prompt_template: None,
            items: 1,
            estimated_input_tokens: 0,
            max_output_tokens: None,
            active_requests: active,
            target_concurrency: 4,
            runtime_config_revision: None,
            provider_max_attempts: None,
            effective_timeout_seconds: None,
            timestamp_ms: ts,
        };
        state.fold(&started(3, 1));
        assert_eq!(state.active_requests, 3);
        // No RequestFinished folded (dropped); next start still reports the truth.
        state.fold(&started(2, 2));
        assert_eq!(state.active_requests, 2);
    }
}
