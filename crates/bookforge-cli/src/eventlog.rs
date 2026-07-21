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
use tracing::warn;

/// Progress events contain metadata rather than book text, so 256 KiB is a
/// generous per-record ceiling that still bounds an unterminated-line buffer.
const MAX_EVENT_LOG_LINE_BYTES: usize = 256 * 1024;
/// Limit replay work and allocations per UI tick while still allowing a busy
/// log to catch up quickly over successive polls.
const MAX_EVENT_LOG_BYTES_PER_POLL: usize = 4 * 1024 * 1024;
const EVENT_LOG_READ_CHUNK_BYTES: usize = 64 * 1024;

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
    discarding_oversized_line: bool,
}

impl EventLogTailer {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            file: None,
            buf: Vec::new(),
            discarding_oversized_line: false,
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
        if self.file.is_none() {
            return Ok(events);
        }

        let mut bytes_read = 0;
        let mut chunk = [0_u8; EVENT_LOG_READ_CHUNK_BYTES];
        while bytes_read < MAX_EVENT_LOG_BYTES_PER_POLL {
            let remaining = MAX_EVENT_LOG_BYTES_PER_POLL - bytes_read;
            let read = self
                .file
                .as_mut()
                .expect("event log was opened above")
                .read(&mut chunk[..remaining.min(EVENT_LOG_READ_CHUNK_BYTES)])?;
            if read == 0 {
                break;
            }
            bytes_read += read;
            self.consume_bytes(&chunk[..read], &mut events);
        }
        Ok(events)
    }

    fn consume_bytes(&mut self, mut bytes: &[u8], events: &mut Vec<ProgressEvent>) {
        while !bytes.is_empty() {
            if self.discarding_oversized_line {
                let Some(newline) = bytes.iter().position(|&byte| byte == b'\n') else {
                    return;
                };
                self.discarding_oversized_line = false;
                bytes = &bytes[newline + 1..];
                continue;
            }

            if let Some(newline) = bytes.iter().position(|&byte| byte == b'\n') {
                let fragment = &bytes[..newline];
                if self.buf.len() + fragment.len() > MAX_EVENT_LOG_LINE_BYTES {
                    self.buf.clear();
                    self.report_oversized_line();
                } else {
                    self.buf.extend_from_slice(fragment);
                    if !self.buf.is_empty()
                        && let Ok(event) = serde_json::from_slice::<ProgressEvent>(&self.buf)
                    {
                        events.push(event);
                    }
                    self.buf.clear();
                }
                bytes = &bytes[newline + 1..];
            } else {
                if self.buf.len() + bytes.len() > MAX_EVENT_LOG_LINE_BYTES {
                    self.buf.clear();
                    self.discarding_oversized_line = true;
                    self.report_oversized_line();
                } else {
                    self.buf.extend_from_slice(bytes);
                }
                return;
            }
        }
    }

    fn report_oversized_line(&self) {
        warn!(
            path = %self.path.display(),
            max_bytes = MAX_EVENT_LOG_LINE_BYTES,
            "event log line exceeds limit; skipping"
        );
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

    #[test]
    fn oversized_complete_line_is_skipped_without_hiding_later_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let expected = serde_json::to_string(&stage_started("setup", 1)).unwrap();
        {
            let mut file = File::create(&path).unwrap();
            file.write_all(&vec![b'x'; MAX_EVENT_LOG_LINE_BYTES + 1])
                .unwrap();
            writeln!(file).unwrap();
            writeln!(file, "{expected}").unwrap();
        }

        let mut tailer = EventLogTailer::new(path);
        let events = tailer.poll().unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            ProgressEvent::StageStarted { stage, timestamp_ms: 1 } if stage == "setup"
        ));
    }

    #[test]
    fn oversized_unterminated_line_never_grows_the_partial_buffer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        {
            let mut file = File::create(&path).unwrap();
            file.write_all(&vec![b'x'; MAX_EVENT_LOG_LINE_BYTES + 1])
                .unwrap();
        }

        let mut tailer = EventLogTailer::new(path.clone());
        assert!(tailer.poll().unwrap().is_empty());
        assert!(tailer.buf.len() <= MAX_EVENT_LOG_LINE_BYTES);
        assert!(tailer.discarding_oversized_line);

        let expected = serde_json::to_string(&stage_started("translating", 2)).unwrap();
        {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(file).unwrap();
            writeln!(file, "{expected}").unwrap();
        }

        let events = tailer.poll().unwrap();
        assert_eq!(events.len(), 1);
        assert!(!tailer.discarding_oversized_line);
    }
}
