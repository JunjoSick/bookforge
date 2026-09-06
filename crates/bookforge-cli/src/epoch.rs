//! Epoch-aware [`RunState`] folding (UI-9/10).
//!
//! A job's event log can contain several *run epochs*: the original
//! `translate`, plus every `resume`/retry wave appended to the same log. Each
//! epoch starts with a `JobCreated` (or a live `JobResumed`). Two hazards come
//! from folding such a log naively:
//!
//! 1. **Poisoned rate/ETA** — `RunState::segments_per_minute` divides total
//!    completed segments by the span from the *first* folded timestamp, which
//!    reaches back across idle time and earlier epochs. A resume that instantly
//!    marks thousands of cache hits done then reports absurd throughput.
//! 2. **Mixed time domains** — the progress gauge counts absolute
//!    done/total segments across all epochs, while stats must describe the
//!    current epoch's throughput.
//!
//! [`EpochTracker`] fixes both without touching core: it resets the timing
//! baseline at each epoch boundary and remembers how many segments were
//! already done when the epoch began, so rate/ETA are computed over
//! *(done − carried)* within *(now − epoch start)* while gauges keep showing
//! absolute progress.

use bookforge_core::{ProgressEvent, RunState};

#[derive(Debug, Default, Clone)]
pub(crate) struct EpochTracker {
    /// Segments that were already terminal before the current epoch began.
    carried_done: usize,
    /// True once any `JobCreated` has been folded (the first JobCreated is the
    /// start of epoch one, not a boundary onto epoch two).
    started: bool,
}

impl EpochTracker {
    pub(crate) fn fold(&mut self, state: &mut RunState, event: &ProgressEvent) {
        let boundary = match event {
            // Live fast-resume: the worker parks/pauses and re-runs in-process.
            ProgressEvent::JobResumed { .. } => true,
            // Appended epochs begin with a fresh JobCreated in the same log.
            ProgressEvent::JobCreated { .. } => self.started,
            _ => false,
        };
        if matches!(event, ProgressEvent::JobCreated { .. }) {
            self.started = true;
        }
        let reset_to = match event {
            _ if boundary => Some(bookforge_core::progress::event_timestamp_ms(event)),
            _ => None,
        };
        state.fold(event);
        if boundary {
            // The window restarts at the epoch boundary so idle gaps between
            // epochs and earlier-epoch work fall out of rate/ETA entirely.
            self.carried_done = state.done_segments;
            // Rebaseline the timing window to the epoch start. A pathological
            // zero timestamp leaves the previous baseline untouched rather
            // than erasing all timing.
            if let Some(ts) = reset_to.filter(|ts| *ts != 0) {
                state.first_timestamp_ms = Some(ts);
                state.last_timestamp_ms = Some(ts);
            }
        }
    }

    fn epoch_done(&self, state: &RunState) -> usize {
        state.done_segments.saturating_sub(self.carried_done)
    }

    /// Throughput of the *current* epoch only (UI-9). Identical to
    /// `RunState::segments_per_minute` for single-epoch logs.
    pub(crate) fn segments_per_minute(&self, state: &RunState) -> f64 {
        let secs = state.elapsed_secs();
        if secs <= 0.0 {
            return 0.0;
        }
        self.epoch_done(state) as f64 / secs * 60.0
    }

    /// ETA derived from current-epoch throughput; remaining work stays
    /// absolute because the gauge is absolute.
    pub(crate) fn eta_secs(&self, state: &RunState) -> f64 {
        let per_min = self.segments_per_minute(state);
        if per_min <= 0.0 {
            return 0.0;
        }
        state.remaining() as f64 / (per_min / 60.0)
    }

    #[cfg(test)]
    fn carried(&self) -> usize {
        self.carried_done
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn created(ts: u64) -> ProgressEvent {
        ProgressEvent::JobCreated {
            job_id: "job_e".into(),
            input_path: "in.epub".into(),
            output_path: "out.epub".into(),
            timestamp_ms: ts,
        }
    }

    fn seg_done(id: &str, ts: u64) -> ProgressEvent {
        ProgressEvent::SegmentFinished {
            segment_id: id.into(),
            status: "succeeded".into(),
            input_tokens: None,
            output_tokens: None,
            timestamp_ms: ts,
        }
    }

    #[test]
    fn first_epoch_matches_plain_runstate_semantics() {
        let mut state = RunState::default();
        let mut tracker = EpochTracker::default();
        tracker.fold(&mut state, &created(10_000));
        tracker.fold(
            &mut state,
            &ProgressEvent::SegmentationFinished {
                segment_count: 4,
                timestamp_ms: 11_000,
            },
        );
        for (i, ts) in [40_000u64, 70_000].into_iter().enumerate() {
            tracker.fold(&mut state, &seg_done(&format!("a{i}"), ts));
        }
        assert_eq!(tracker.carried(), 0);
        assert!((state.elapsed_secs() - 60.0).abs() < 1e-9);
        assert!((tracker.segments_per_minute(&state) - 2.0).abs() < 1e-9);
        // 2 remaining at 2/min -> ~60s ETA.
        assert!((tracker.eta_secs(&state) - 60.0).abs() < 1e-9);
    }

    #[test]
    fn resumed_epoch_rebaselines_rate_and_excludes_carried_segments() {
        // Epoch 1: 3 slow completions (10 s apart), then an interruption.
        let mut state = RunState::default();
        let mut tracker = EpochTracker::default();
        tracker.fold(&mut state, &created(1_000));
        tracker.fold(
            &mut state,
            &ProgressEvent::SegmentationFinished {
                segment_count: 6,
                timestamp_ms: 1_100,
            },
        );
        for i in 0..3 {
            tracker.fold(&mut state, &seg_done(&format!("a{i}"), (i + 1) * 10_000));
        }
        assert_eq!(state.done_segments, 3);

        // Idle gap of more than a day, then the resume epoch lands with
        // instant cache hits followed by fresh work.
        tracker.fold(&mut state, &created(100_000));
        tracker.fold(
            &mut state,
            &ProgressEvent::CacheScanFinished {
                hits: 3,
                misses: 3,
                timestamp_ms: 100_100,
            },
        );
        tracker.fold(&mut state, &seg_done("b0", 160_000));

        // Carried 3 excluded; the 60 s window saw exactly one fresh segment.
        assert_eq!(tracker.carried(), 3);
        assert_eq!(state.done_segments, 4);
        assert_eq!(tracker.epoch_done(&state), 1);
        assert!(
            (tracker.segments_per_minute(&state) - 1.0).abs() < 1e-9,
            "rate must be computed over the resumed epoch only, got {}",
            tracker.segments_per_minute(&state)
        );
        // Remaining 2 segments at 1/min → 120 s.
        assert!((tracker.eta_secs(&state) - 120.0).abs() < 1e-9);
        // The gauge stays absolute.
        assert_eq!(
            (state.done_segments as f64 / state.total_segments as f64 * 100.0).round() as u32,
            67
        );
    }

    #[test]
    fn job_resumed_also_starts_a_new_epoch_window() {
        let mut state = RunState::default();
        let mut tracker = EpochTracker::default();
        tracker.fold(&mut state, &created(1_000));
        tracker.fold(&mut state, &seg_done("a0", 5_000));
        tracker.fold(
            &mut state,
            &ProgressEvent::JobResumed {
                job_id: "job_e".into(),
                timestamp_ms: 90_000,
            },
        );
        assert_eq!(tracker.carried(), 1);
        // 1 fresh segment over the following 30 s of window time.
        tracker.fold(&mut state, &seg_done("b0", 120_000));
        assert!((tracker.segments_per_minute(&state) - 2.0).abs() < 1e-9);
    }

    #[test]
    fn zero_epoch_timestamp_leaves_baseline_unset_rather_than_garbage() {
        let mut state = RunState::default();
        let mut tracker = EpochTracker::default();
        tracker.fold(&mut state, &created(50_000));
        tracker.fold(&mut state, &seg_done("a0", 51_000));
        // Pathological zero-timestamp boundary: baseline simply clears.
        tracker.fold(
            &mut state,
            &ProgressEvent::JobCreated {
                job_id: "job_e".into(),
                input_path: "in.epub".into(),
                output_path: "out.epub".into(),
                timestamp_ms: 0,
            },
        );
        assert_eq!(tracker.carried(), 1);
        assert_eq!(state.first_timestamp_ms, Some(50_000));
    }
}
