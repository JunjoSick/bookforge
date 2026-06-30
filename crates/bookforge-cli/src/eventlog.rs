//! Shared event-log tailing for the live UIs.
//!
//! Both `watch` (terminal) and `serve` (web) follow a job's append-only
//! `events.jsonl` and fold it into [`bookforge_core::RunState`]. This module is
//! the single home for resolving a job's log path and for the byte-offset tailer
//! that tolerates a partial trailing line and a not-yet-created file.

use std::{fs::File, io::Read, path::PathBuf};

use anyhow::Result;
use bookforge_core::ProgressEvent;
use bookforge_store::JobRecord;

/// Resolve a job's event-log path, mirroring the fallback the store uses: prefer
/// the recorded `events_path`, otherwise the conventional run directory.
pub fn events_path_for(job: Option<&JobRecord>, job_id: &str) -> PathBuf {
    job.and_then(|j| j.events_path.clone())
        .unwrap_or_else(|| PathBuf::from(format!(".bookforge/runs/{job_id}/events.jsonl")))
}

/// Incremental reader over a job's `events.jsonl`.
///
/// Each [`poll`](EventLogTailer::poll) reads newly-appended bytes and returns the
/// events parsed from any complete lines. A partial trailing line is held in the
/// buffer until its newline arrives; a missing file simply yields nothing and is
/// retried on the next poll. The very first `poll` against an existing log walks
/// it from the start, so the same tailer serves both an initial replay snapshot
/// and live follow.
pub struct EventLogTailer {
    path: PathBuf,
    file: Option<File>,
    buf: Vec<u8>,
}

impl EventLogTailer {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            file: None,
            buf: Vec::new(),
        }
    }

    /// Read appended bytes and parse each complete JSONL line into an event.
    pub fn poll(&mut self) -> Result<Vec<ProgressEvent>> {
        let mut events = Vec::new();

        if self.file.is_none() {
            match File::open(&self.path) {
                Ok(opened) => self.file = Some(opened),
                Err(_) => return Ok(events),
            }
        }
        let Some(handle) = self.file.as_mut() else {
            return Ok(events);
        };

        let mut chunk = Vec::new();
        handle.read_to_end(&mut chunk)?;
        if chunk.is_empty() {
            return Ok(events);
        }
        self.buf.extend_from_slice(&chunk);

        let mut start = 0;
        while let Some(offset) = self.buf[start..].iter().position(|&b| b == b'\n') {
            let end = start + offset;
            let line = &self.buf[start..end];
            if !line.is_empty()
                && let Ok(event) = serde_json::from_slice::<ProgressEvent>(line)
            {
                events.push(event);
            }
            start = end + 1;
        }
        self.buf.drain(..start);
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn stage_started(stage: &str, ts: u64) -> ProgressEvent {
        ProgressEvent::StageStarted {
            stage: stage.to_string(),
            timestamp_ms: ts,
        }
    }

    #[test]
    fn missing_file_yields_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut tailer = EventLogTailer::new(dir.path().join("events.jsonl"));
        assert!(tailer.poll().unwrap().is_empty());
    }

    #[test]
    fn tails_incrementally_and_tolerates_partial_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut tailer = EventLogTailer::new(path.clone());

        let first = serde_json::to_string(&stage_started("setup", 1)).unwrap();
        let second = serde_json::to_string(&stage_started("translating", 2)).unwrap();

        // One complete line plus a partial line with no trailing newline yet.
        {
            let mut file = File::create(&path).unwrap();
            writeln!(file, "{first}").unwrap();
            write!(file, "{second}").unwrap();
        }
        let batch = tailer.poll().unwrap();
        assert_eq!(batch.len(), 1, "partial trailing line must be withheld");

        // Completing the partial line releases the second event on the next poll.
        {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(file).unwrap();
        }
        let batch = tailer.poll().unwrap();
        assert_eq!(batch.len(), 1);

        // No new bytes: nothing more.
        assert!(tailer.poll().unwrap().is_empty());
    }
}
