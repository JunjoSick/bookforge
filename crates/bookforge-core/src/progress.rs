pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

use std::collections::VecDeque;

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

/// Returns the `timestamp_ms` carried by any `ProgressEvent` variant.
pub fn event_timestamp_ms(event: &ProgressEvent) -> u64 {
    use ProgressEvent::*;
    match event {
        JobCreated { timestamp_ms, .. }
        | StageStarted { timestamp_ms, .. }
        | StageFinished { timestamp_ms, .. }
        | RuntimeConfigResolved { timestamp_ms, .. }
        | SegmentationFinished { timestamp_ms, .. }
        | CacheScanFinished { timestamp_ms, .. }
        | BatchQueued { timestamp_ms, .. }
        | BatchSplit { timestamp_ms, .. }
        | BatchRepairStarted { timestamp_ms, .. }
        | BatchRepairFinished { timestamp_ms, .. }
        | RequestStarted { timestamp_ms, .. }
        | RequestFinished { timestamp_ms, .. }
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

    // Tokens.
    pub input_tokens: u64,
    pub output_tokens: u64,

    // Timing (epoch milliseconds, taken from event timestamps).
    pub first_timestamp_ms: Option<u64>,
    pub last_timestamp_ms: Option<u64>,

    // Terminal summary.
    pub finished: bool,
    pub finished_elapsed_ms: Option<u64>,

    // Bounded log surfaces for the TUI / web panels.
    pub recent_events: VecDeque<ProgressEvent>,
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
            }
            ProgressEvent::CacheScanFinished { hits, .. } => {
                self.cached = *hits;
                self.done_segments = *hits;
            }
            ProgressEvent::SegmentFinished {
                segment_id,
                status,
                input_tokens,
                output_tokens,
                ..
            } => {
                match status.as_str() {
                    "succeeded" | "skipped_cached" | "needs_review" | "failed" => {
                        self.done_segments += 1;
                    }
                    _ => {}
                }
                match status.as_str() {
                    "succeeded" => self.succeeded += 1,
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
                if let Some(tokens) = input_tokens {
                    self.input_tokens += *tokens;
                }
                if let Some(tokens) = output_tokens {
                    self.output_tokens += *tokens;
                }
            }
            ProgressEvent::RequestStarted {
                target_concurrency, ..
            } => {
                self.active_requests += 1;
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
            }
            _ => {}
        }

        push_capped_deque(&mut self.recent_events, event.clone(), RECENT_EVENTS_CAP);
    }

    /// Segments not yet in a terminal state.
    pub fn remaining(&self) -> usize {
        self.total_segments.saturating_sub(self.done_segments)
    }

    /// Wall-clock span covered by the folded events, in seconds.
    pub fn elapsed_secs(&self) -> f64 {
        match (self.first_timestamp_ms, self.last_timestamp_ms) {
            (Some(start), Some(end)) if end >= start => (end - start) as f64 / 1000.0,
            _ => 0.0,
        }
    }

    /// Throughput in segments per minute, or 0 before any time has elapsed.
    pub fn segments_per_minute(&self) -> f64 {
        let secs = self.elapsed_secs();
        if secs <= 0.0 {
            return 0.0;
        }
        self.done_segments as f64 / secs * 60.0
    }

    /// Estimated seconds until completion, or 0 when it cannot be estimated.
    pub fn eta_secs(&self) -> f64 {
        let per_min = self.segments_per_minute();
        if per_min <= 0.0 {
            return 0.0;
        }
        self.remaining() as f64 / (per_min / 60.0)
    }
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
}
