//! Canonical presentation layer for run dashboards (UI-31).
//!
//! Four rendering surfaces consumed progress in slightly different ways and
//! drifted: the indicatif bar set (`progress.rs`), the ratatui TUI
//! (`tui/mod.rs`), `tail`'s state reconstruction, and the serve dashboard's
//! SSE folds. Everything they have in common now lives here:
//!
//! - [`RunView`] is the one RunState + [`EpochTracker`] pairing; folding a
//!   live or replayed event anywhere in the crate goes through it, so gauges,
//!   rate/ETA windows, and epoch rebaselining cannot diverge per surface.
//! - The format helpers below are the only number/phase formatters a renderer
//!   may use (ETA buckets, compact token counts, throughput strings, status
//!   vocabulary), replacing the duplicated ETA formatters that had drifted.
//!
//! Deliberately out of scope: surface-specific layout (bar templates, TUI
//! widgets) stays with each renderer.

use std::ops::Deref;

use bookforge_core::{ProgressEvent, RunState};

use crate::epoch::EpochTracker;

/// One canonical fold view over a run's events: the shared [`RunState`]
/// plus epoch-aware timing baselines. All dashboards render from this pair.
#[derive(Debug, Default, Clone)]
pub(crate) struct RunView {
    pub(crate) state: RunState,
    epochs: EpochTracker,
}

impl RunView {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Fold one live or replayed event through the shared epoch-aware
    /// pipeline (UI-9): gauges stay absolute while rate/ETA describe only the
    /// current epoch.
    pub(crate) fn fold(&mut self, event: &ProgressEvent) {
        self.epochs.fold(&mut self.state, event);
    }

    /// Current-epoch throughput in segments per minute.
    pub(crate) fn segments_per_minute(&self) -> f64 {
        self.epochs.segments_per_minute(&self.state)
    }

    /// ETA derived from current-epoch throughput; remaining work is absolute.
    pub(crate) fn eta_secs(&self) -> f64 {
        self.epochs.eta_secs(&self.state)
    }

    /// Gauge ratio clamped to `[0, 1]`; 0 before segmentation is known.
    #[cfg(any(feature = "tui", test))]
    pub(crate) fn progress_ratio(&self) -> f64 {
        if self.state.total_segments > 0 {
            (self.state.done_segments as f64 / self.state.total_segments as f64).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }

    /// Consume the view, returning the folded [`RunState`] (for callers that
    /// hand the state onward, e.g. serialized into an SSE payload).
    #[cfg(feature = "serve")]
    pub(crate) fn into_state(self) -> RunState {
        self.state
    }
}

impl Deref for RunView {
    type Target = RunState;

    fn deref(&self) -> &Self::Target {
        &self.state
    }
}

/// Canonical run-status vocabulary shared by every rendered surface.
pub(crate) fn run_status_name(state: &RunState) -> &'static str {
    if state.finished {
        "done"
    } else if state.paused {
        "paused"
    } else if state.total_segments > 0 {
        "running"
    } else {
        "starting"
    }
}

/// Canonical ETA formatter (merges the two drifted implementations: the bars'
/// bucketing and the TUI's em-dash-when-unknown rule).
pub(crate) fn format_eta(secs: f64) -> String {
    if secs <= 0.0 {
        return "—".to_string();
    }
    if secs > 3600.0 {
        format!("{:.1}h", secs / 3600.0)
    } else if secs > 60.0 {
        format!("{:.0}m", secs / 60.0)
    } else {
        format!("{secs:.0}s")
    }
}

/// Canonical throughput string, e.g. `"12.3 seg/min"`.
pub(crate) fn format_rate(segments_per_minute: f64) -> String {
    format!("{segments_per_minute:.1} seg/min")
}

/// Compact token counter shared by token-bearing surfaces: `999`, `12.3k`,
/// `2.0M`.
#[cfg(any(feature = "tui", test))]
pub(crate) fn format_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn created(ts: u64) -> ProgressEvent {
        ProgressEvent::JobCreated {
            job_id: "job_view".into(),
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

    /// The view must reproduce the plain epoch-tracker semantics on a
    /// single-epoch log (guards against accidental drift when surfaces adopt
    /// the shared layer).
    #[test]
    fn fold_matches_epoch_tracker_semantics() {
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
        tracker.fold(&mut state, &seg_done("a0", 40_000));
        tracker.fold(&mut state, &seg_done("a1", 70_000));

        let mut view = RunView::new();
        view.fold(&created(10_000));
        view.fold(&ProgressEvent::SegmentationFinished {
            segment_count: 4,
            timestamp_ms: 11_000,
        });
        view.fold(&seg_done("a0", 40_000));
        view.fold(&seg_done("a1", 70_000));

        assert_eq!(view.done_segments, state.done_segments);
        assert!((view.segments_per_minute() - tracker.segments_per_minute(&state)).abs() < 1e-9);
        assert!((view.eta_secs() - tracker.eta_secs(&state)).abs() < 1e-9);
        assert_eq!(view.progress_ratio(), 0.5);
    }

    #[test]
    fn resumed_epoch_keeps_gauge_absolute_but_rate_current() {
        let mut view = RunView::new();
        view.fold(&created(1_000));
        view.fold(&ProgressEvent::SegmentationFinished {
            segment_count: 6,
            timestamp_ms: 1_100,
        });
        for i in 0..3u64 {
            view.fold(&seg_done(&format!("a{i}"), (i + 1) * 10_000));
        }
        // Resume epoch appends a fresh JobCreated to the same log.
        view.fold(&created(100_000));
        view.fold(&ProgressEvent::CacheScanFinished {
            hits: 3,
            misses: 3,
            timestamp_ms: 100_100,
        });
        view.fold(&seg_done("b0", 160_000));

        assert_eq!(view.done_segments, 4, "gauge stays absolute across epochs");
        assert!(
            (view.segments_per_minute() - 1.0).abs() < 1e-9,
            "rate describes the current epoch only"
        );
        assert!((view.eta_secs() - 120.0).abs() < 1e-9);
    }

    #[test]
    fn eta_formatting_buckets_by_scale_with_unknown_em_dash() {
        assert_eq!(format_eta(0.0), "—");
        assert_eq!(format_eta(-3.0), "—");
        assert_eq!(format_eta(45.0), "45s");
        assert_eq!(format_eta(150.0), "2m");
        assert_eq!(format_eta(7200.0), "2.0h");
    }

    #[test]
    fn count_formatting_is_compact() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(12_345), "12.3k");
        assert_eq!(format_count(2_000_000), "2.0M");
    }

    #[test]
    fn rate_formatting_is_stable() {
        assert_eq!(format_rate(0.0), "0.0 seg/min");
        assert_eq!(format_rate(12.34), "12.3 seg/min");
    }

    #[test]
    fn status_names_come_from_one_vocabulary() {
        let mut state = RunState::default();
        assert_eq!(run_status_name(&state), "starting");
        state.fold(&ProgressEvent::SegmentationFinished {
            segment_count: 2,
            timestamp_ms: 10,
        });
        assert_eq!(run_status_name(&state), "running");
        state.fold(&ProgressEvent::JobPaused {
            job_id: "job_view".into(),
            timestamp_ms: 20,
        });
        assert_eq!(run_status_name(&state), "paused");
        state.fold(&ProgressEvent::TranslationFinished {
            succeeded: 2,
            cached: 0,
            needs_review: 0,
            failed: 0,
            input_tokens: 0,
            output_tokens: 0,
            elapsed_ms: 5,
            timestamp_ms: 30,
        });
        assert_eq!(run_status_name(&state), "done");
    }
}
