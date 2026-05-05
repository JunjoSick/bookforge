use std::{
    io::{BufWriter, IsTerminal, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

use anyhow::Result;
use bookforge_core::{ProgressEvent, ProgressSink};
use tokio::{sync::mpsc, task::JoinHandle};

pub const PROGRESS_EVENT_QUEUE_CAPACITY: usize = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum UiMode {
    Auto,
    Progress,
    Json,
    Quiet,
}

/// A progress sink that sends events over a bounded mpsc channel using
/// try_send. If the channel is full, events are dropped and counted.
pub struct ChannelProgressSink {
    tx: mpsc::Sender<ProgressEvent>,
    dropped: Arc<AtomicUsize>,
}

impl ChannelProgressSink {
    pub fn new(tx: mpsc::Sender<ProgressEvent>, dropped: Arc<AtomicUsize>) -> Self {
        Self { tx, dropped }
    }

    #[allow(dead_code)]
    pub fn dropped_count(&self) -> usize {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl ProgressSink for ChannelProgressSink {
    fn emit(&self, event: ProgressEvent) {
        match self.tx.try_send(event) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) | Err(mpsc::error::TrySendError::Closed(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// ProgressReporter spawns a background task that consumes ProgressEvents
/// and renders a terminal dashboard (progress mode) or writes JSONL lines.
pub struct ProgressReporter {
    tx: mpsc::Sender<ProgressEvent>,
    join: JoinHandle<Result<()>>,
    dropped: Arc<AtomicUsize>,
}

impl ProgressReporter {
    pub fn spawn(ui_mode: UiMode, jsonl_path: Option<PathBuf>) -> Self {
        let (tx, rx) = mpsc::channel::<ProgressEvent>(PROGRESS_EVENT_QUEUE_CAPACITY);
        let dropped = Arc::new(AtomicUsize::new(0));
        let dropped_clone = dropped.clone();

        let join = tokio::spawn(render_loop(rx, ui_mode, jsonl_path, dropped_clone));

        Self { tx, join, dropped }
    }

    pub fn sink(&self) -> Arc<dyn ProgressSink> {
        Arc::new(ChannelProgressSink::new(
            self.tx.clone(),
            self.dropped.clone(),
        ))
    }

    pub async fn shutdown(self) -> Result<()> {
        drop(self.tx);
        self.join
            .await
            .map_err(|e| anyhow::anyhow!("progress reporter join error: {e}"))??;
        Ok(())
    }
}

async fn render_loop(
    mut rx: mpsc::Receiver<ProgressEvent>,
    ui_mode: UiMode,
    jsonl_path: Option<PathBuf>,
    dropped: Arc<AtomicUsize>,
) -> Result<()> {
    let render_mode = resolve_render_mode(ui_mode, std::io::stderr().is_terminal());
    let mut file_writer = JsonlFileWriter::new(jsonl_path);
    let mut renderer = Renderer::new(render_mode)?;

    while let Some(event) = rx.recv().await {
        file_writer.write_event(&event)?;
        renderer.handle_event(&event)?;
    }

    file_writer.flush()?;
    renderer.finish()?;

    let d = dropped.load(Ordering::Relaxed);
    if d > 0 {
        eprintln!("({d} progress events dropped)");
    }

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenderMode {
    Quiet,
    Progress,
    JsonStdout,
}

fn resolve_render_mode(ui_mode: UiMode, stderr_is_tty: bool) -> RenderMode {
    match ui_mode {
        UiMode::Auto if stderr_is_tty => RenderMode::Progress,
        UiMode::Auto => RenderMode::Quiet,
        UiMode::Progress => RenderMode::Progress,
        UiMode::Json => RenderMode::JsonStdout,
        UiMode::Quiet => RenderMode::Quiet,
    }
}

struct JsonlFileWriter {
    path: Option<PathBuf>,
    writer: Option<BufWriter<std::fs::File>>,
    failed: bool,
    last_flush: Instant,
}

impl JsonlFileWriter {
    fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            writer: None,
            failed: false,
            last_flush: Instant::now(),
        }
    }

    fn ensure_open(&mut self) -> Result<()> {
        if self.writer.is_some() || self.failed {
            return Ok(());
        }
        let Some(path) = self.path.as_ref() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::File::create(path) {
            Ok(file) => {
                self.writer = Some(BufWriter::new(file));
                self.last_flush = Instant::now();
            }
            Err(err) => {
                self.failed = true;
                eprintln!(
                    "warn: cannot create JSONL progress log {}: {err}",
                    path.display()
                );
            }
        }
        Ok(())
    }

    fn write_event(&mut self, event: &ProgressEvent) -> Result<()> {
        self.ensure_open()?;
        let Some(writer) = self.writer.as_mut() else {
            return Ok(());
        };
        writeln!(writer, "{}", serde_json::to_string(event)?)?;
        if is_important_event(event)
            || self.last_flush.elapsed() >= std::time::Duration::from_secs(2)
        {
            writer.flush()?;
            self.last_flush = Instant::now();
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if let Some(writer) = self.writer.as_mut() {
            writer.flush()?;
        }
        Ok(())
    }
}

enum Renderer {
    Quiet,
    JsonStdout,
    Progress(ProgressBars),
}

impl Renderer {
    fn new(mode: RenderMode) -> Result<Self> {
        match mode {
            RenderMode::Quiet => Ok(Renderer::Quiet),
            RenderMode::JsonStdout => Ok(Renderer::JsonStdout),
            RenderMode::Progress => Ok(Renderer::Progress(ProgressBars::new()?)),
        }
    }

    fn handle_event(&mut self, event: &ProgressEvent) -> Result<()> {
        match self {
            Renderer::Quiet => Ok(()),
            Renderer::JsonStdout => {
                println!("{}", serde_json::to_string(event)?);
                Ok(())
            }
            Renderer::Progress(bars) => bars.handle_event(event),
        }
    }

    fn finish(&mut self) -> Result<()> {
        match self {
            Renderer::Progress(bars) => bars.finish(),
            _ => Ok(()),
        }
    }
}

struct ProgressBars {
    multi: indicatif::MultiProgress,
    stage_bar: indicatif::ProgressBar,
    seg_bar: indicatif::ProgressBar,
    batch_bar: indicatif::ProgressBar,
    rate_bar: indicatif::ProgressBar,
    checkpoint_bar: indicatif::ProgressBar,
    start: Instant,
    total_segments: usize,
    done_segments: usize,
    cached: usize,
    active_requests: usize,
    _checkpoint_flushed: usize,
}

impl ProgressBars {
    fn new() -> Result<Self> {
        use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

        let multi = MultiProgress::new();
        let stage_bar = multi.add(
            ProgressBar::new_spinner()
                .with_style(
                    ProgressStyle::with_template("{spinner:.green} {msg}")
                        .unwrap()
                        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
                )
                .with_message("Starting..."),
        );
        let seg_bar = multi.add(
            ProgressBar::new(0)
                .with_style(
                    ProgressStyle::with_template(
                        "  Segments: [{bar:20.cyan/blue}] {pos}/{len} ({msg})",
                    )
                    .unwrap(),
                )
                .with_message("0 cached"),
        );
        let batch_bar = multi.add(
            ProgressBar::new_spinner()
                .with_style(
                    ProgressStyle::with_template("{spinner:.yellow} Batches: {msg}")
                        .unwrap()
                        .tick_strings(&["-", "\\", "|", "/"]),
                )
                .with_message("queuing..."),
        );
        let rate_bar = multi.add(
            ProgressBar::new_spinner()
                .with_style(ProgressStyle::with_template("  {msg}").unwrap())
                .with_message(""),
        );
        let checkpoint_bar = multi.add(
            ProgressBar::new_spinner()
                .with_style(ProgressStyle::with_template("  Checkpoint: {msg}").unwrap())
                .with_message("flushed 0"),
        );

        Ok(Self {
            multi,
            stage_bar,
            seg_bar,
            batch_bar,
            rate_bar,
            checkpoint_bar,
            start: Instant::now(),
            total_segments: 0,
            done_segments: 0,
            cached: 0,
            active_requests: 0,
            _checkpoint_flushed: 0,
        })
    }

    fn handle_event(&mut self, event: &ProgressEvent) -> Result<()> {
        match event {
            ProgressEvent::StageStarted { stage, .. } => {
                self.stage_bar.set_message(format!("{stage}..."));
            }
            ProgressEvent::StageFinished { .. } => {
                self.stage_bar.set_message("translating...");
                self.stage_bar
                    .enable_steady_tick(std::time::Duration::from_millis(80));
            }
            ProgressEvent::SegmentationFinished { segment_count, .. } => {
                self.total_segments = *segment_count;
                self.seg_bar.set_length(self.total_segments as u64);
                self.seg_bar.set_message(format!("{} cached", self.cached));
            }
            ProgressEvent::CacheScanFinished { hits, .. } => {
                self.cached = *hits;
                self.seg_bar.set_message(format!("{} cached", self.cached));
            }
            ProgressEvent::SegmentFinished { status, .. } => match status.as_str() {
                "succeeded" | "skipped_cached" | "needs_review" | "failed" => {
                    self.done_segments += 1;
                    self.seg_bar.set_position(self.done_segments as u64);
                    let elapsed = self.start.elapsed().as_secs_f64().max(0.1);
                    let rate_per_min = self.done_segments as f64 / elapsed * 60.0;
                    let remaining = self.total_segments.saturating_sub(self.done_segments);
                    let eta_secs = if rate_per_min > 0.0 {
                        remaining as f64 / (rate_per_min / 60.0)
                    } else {
                        0.0
                    };
                    let eta_str = if eta_secs > 3600.0 {
                        format!("{:.1}h", eta_secs / 3600.0)
                    } else if eta_secs > 60.0 {
                        format!("{:.0}m", eta_secs / 60.0)
                    } else {
                        format!("{:.0}s", eta_secs)
                    };
                    self.rate_bar.set_message(format!(
                        "{}/{} done, {:.1} seg/min, ETA {eta_str}",
                        self.done_segments, self.total_segments, rate_per_min
                    ));
                }
                _ => {}
            },
            ProgressEvent::RequestStarted { .. } => {
                self.active_requests += 1;
                self.batch_bar
                    .set_message(format!("{} active", self.active_requests));
            }
            ProgressEvent::RequestFinished { .. } => {
                self.active_requests = self.active_requests.saturating_sub(1);
                self.batch_bar
                    .set_message(format!("{} active", self.active_requests));
            }
            ProgressEvent::CheckpointFlushed { flushed_count, .. } => {
                self._checkpoint_flushed = *flushed_count;
                self.checkpoint_bar
                    .set_message(format!("flushed {}", self._checkpoint_flushed));
            }
            ProgressEvent::BatchQueued { batch_id, .. } => {
                self.batch_bar
                    .set_message(format!("batch {batch_id} queued"));
            }
            ProgressEvent::BatchSplit { batch_id, .. } => {
                self.batch_bar
                    .set_message(format!("batch {batch_id} split"));
            }
            ProgressEvent::Warning { message, .. } => {
                self.multi.println(format!("  [warn] {message}")).ok();
            }
            ProgressEvent::Error { message, .. } => {
                self.multi.println(format!("  [error] {message}")).ok();
            }
            ProgressEvent::TranslationFinished {
                succeeded,
                cached: c,
                needs_review,
                failed,
                ..
            } => {
                let done = *succeeded + *c;
                self.seg_bar.set_position(done as u64);
                self.seg_bar.finish_with_message(format!(
                    "{done} done, {} needs review, {} failed",
                    *needs_review, *failed
                ));
                self.stage_bar.finish_and_clear();
                self.batch_bar.finish_and_clear();
                self.rate_bar.finish_and_clear();
                self.checkpoint_bar.finish_and_clear();
            }
            _ => {}
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        self.multi.clear().ok();
        Ok(())
    }
}

fn is_important_event(event: &ProgressEvent) -> bool {
    match event {
        ProgressEvent::Error { .. }
        | ProgressEvent::Warning { .. }
        | ProgressEvent::BatchRepairFinished { .. }
        | ProgressEvent::CheckpointFlushed { .. }
        | ProgressEvent::TranslationFinished { .. }
        | ProgressEvent::DroppedEvents { .. } => true,
        ProgressEvent::RequestFinished { status, .. } => status != "ok",
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_auto_uses_progress_when_tty() {
        assert_eq!(
            resolve_render_mode(UiMode::Auto, true),
            RenderMode::Progress
        );
    }

    #[test]
    fn ui_auto_uses_quiet_when_not_tty() {
        assert_eq!(resolve_render_mode(UiMode::Auto, false), RenderMode::Quiet);
    }

    #[test]
    fn ui_progress_always_uses_progress() {
        assert_eq!(
            resolve_render_mode(UiMode::Progress, false),
            RenderMode::Progress
        );
        assert_eq!(
            resolve_render_mode(UiMode::Progress, true),
            RenderMode::Progress
        );
    }

    #[test]
    fn ui_json_always_uses_json_stdout() {
        assert_eq!(
            resolve_render_mode(UiMode::Json, false),
            RenderMode::JsonStdout
        );
        assert_eq!(
            resolve_render_mode(UiMode::Json, true),
            RenderMode::JsonStdout
        );
    }

    #[test]
    fn ui_quiet_always_uses_quiet() {
        assert_eq!(resolve_render_mode(UiMode::Quiet, false), RenderMode::Quiet);
        assert_eq!(resolve_render_mode(UiMode::Quiet, true), RenderMode::Quiet);
    }

    /// With --progress-jsonl set, events are written to file regardless of
    /// quiet mode (no progress bars, no stdout).
    #[tokio::test]
    async fn progress_jsonl_writes_file_in_quiet_mode() {
        let path =
            std::env::temp_dir().join(format!("bookforge-test-quiet-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let (tx, rx) = mpsc::channel::<ProgressEvent>(16);

        // Spawn reporter in quiet mode with JSONL file
        let reporter_task = render_loop(
            rx,
            UiMode::Quiet,
            Some(path.clone()),
            Arc::new(AtomicUsize::new(0)),
        );
        let handle = tokio::spawn(reporter_task);

        tx.send(ProgressEvent::StageStarted {
            stage: "test".to_string(),
            timestamp_ms: 0,
        })
        .await
        .unwrap();
        tx.send(ProgressEvent::StageFinished {
            stage: "test".to_string(),
            timestamp_ms: 0,
        })
        .await
        .unwrap();
        drop(tx);

        handle.await.unwrap().unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("StageStarted"));
        assert!(content.contains("StageFinished"));
    }

    /// With --progress-jsonl and --ui json, both stdout JSON and file are emitted.
    #[tokio::test]
    async fn progress_jsonl_writes_file_in_json_stdout_mode() {
        let path =
            std::env::temp_dir().join(format!("bookforge-test-json-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let (tx, rx) = mpsc::channel::<ProgressEvent>(16);

        let reporter_task = render_loop(
            rx,
            UiMode::Json,
            Some(path.clone()),
            Arc::new(AtomicUsize::new(0)),
        );
        let handle = tokio::spawn(reporter_task);

        tx.send(ProgressEvent::StageStarted {
            stage: "json_test".to_string(),
            timestamp_ms: 0,
        })
        .await
        .unwrap();
        drop(tx);

        handle.await.unwrap().unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("StageStarted"));
    }

    /// With --progress-jsonl and --ui progress, both bars and file are emitted.
    #[tokio::test]
    async fn progress_jsonl_writes_file_in_progress_mode() {
        let path = std::env::temp_dir().join(format!(
            "bookforge-test-progress-{}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let (tx, rx) = mpsc::channel::<ProgressEvent>(16);

        let reporter_task = render_loop(
            rx,
            UiMode::Progress,
            Some(path.clone()),
            Arc::new(AtomicUsize::new(0)),
        );
        let handle = tokio::spawn(reporter_task);

        tx.send(ProgressEvent::Warning {
            kind: "test".to_string(),
            message: "testing".to_string(),
            timestamp_ms: 0,
        })
        .await
        .unwrap();
        drop(tx);

        handle.await.unwrap().unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("Warning"));
    }
}
