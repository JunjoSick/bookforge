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
    let effective_mode = match ui_mode {
        UiMode::Auto if std::io::stderr().is_terminal() => UiMode::Progress,
        UiMode::Auto => UiMode::Quiet,
        other => other,
    };

    match effective_mode {
        UiMode::Quiet => while rx.recv().await.is_some() {},
        UiMode::Json => {
            render_jsonl_stdout(&mut rx).await?;
        }
        UiMode::Progress | UiMode::Auto => {
            render_progress_bars(&mut rx, jsonl_path, &dropped).await?;
        }
    }
    Ok(())
}

async fn render_progress_bars(
    rx: &mut mpsc::Receiver<ProgressEvent>,
    jsonl_path: Option<PathBuf>,
    dropped: &Arc<AtomicUsize>,
) -> Result<()> {
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

    let start = Instant::now();
    let mut total_segments = 0usize;
    let mut done_segments = 0usize;
    let mut cached = 0usize;
    let mut active_requests = 0usize;
    let mut _checkpoint_flushed = 0usize;
    let mut last_render = Instant::now();
    let mut jsonl_writer: Option<BufWriter<std::fs::File>> = None;
    let mut jsonl_failed = false;
    let mut jsonl_last_flush = Instant::now();

    loop {
        // Receive with a short timeout so we can render periodically
        let event = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await;

        match event {
            Ok(Some(event)) => {
                // Write to JSONL if enabled
                let is_critical = is_important_event(&event);
                if let Some(ref mut w) = jsonl_writer {
                    let _ = writeln!(w, "{}", serde_json::to_string(&event).unwrap_or_default());
                    if is_critical || jsonl_last_flush.elapsed() > std::time::Duration::from_secs(2)
                    {
                        let _ = w.flush();
                        jsonl_last_flush = Instant::now();
                    }
                }

                match &event {
                    ProgressEvent::StageStarted { stage, .. } => {
                        stage_bar.set_message(format!("{stage}..."));
                    }
                    ProgressEvent::StageFinished { .. } => {
                        stage_bar.set_message("translating...");
                        stage_bar.enable_steady_tick(std::time::Duration::from_millis(80));
                    }
                    ProgressEvent::SegmentationFinished { segment_count, .. } => {
                        total_segments = *segment_count;
                        seg_bar.set_length(total_segments as u64);
                        seg_bar.set_message(format!("{cached} cached"));
                    }
                    ProgressEvent::CacheScanFinished { hits, .. } => {
                        cached = *hits;
                        seg_bar.set_message(format!("{cached} cached"));
                    }
                    ProgressEvent::SegmentFinished { status, .. } => {
                        // Count all terminal statuses as completed
                        match status.as_str() {
                            "succeeded" | "skipped_cached" | "needs_review" | "failed" => {
                                done_segments += 1;
                                seg_bar.set_position(done_segments as u64);
                                let elapsed = start.elapsed().as_secs_f64().max(0.1);
                                let rate_per_min = done_segments as f64 / elapsed * 60.0;
                                rate_bar.set_message(format!(
                                    "{done_segments}/{total_segments} done, {rate_per_min:.1} seg/min"
                                ));
                            }
                            _ => {}
                        }
                    }
                    ProgressEvent::RequestStarted { .. } => {
                        active_requests += 1;
                        batch_bar.set_message(format!("{active_requests} active"));
                    }
                    ProgressEvent::RequestFinished { .. } => {
                        active_requests = active_requests.saturating_sub(1);
                        batch_bar.set_message(format!("{active_requests} active"));
                    }
                    ProgressEvent::CheckpointFlushed { flushed_count, .. } => {
                        _checkpoint_flushed = *flushed_count;
                        checkpoint_bar.set_message(format!("flushed {_checkpoint_flushed}"));
                    }
                    ProgressEvent::BatchQueued { batch_id, .. } => {
                        batch_bar.set_message(format!("batch {batch_id} queued"));
                    }
                    ProgressEvent::BatchSplit { batch_id, .. } => {
                        batch_bar.set_message(format!("batch {batch_id} split"));
                    }
                    ProgressEvent::Warning { message, .. } => {
                        multi.println(format!("  [warn] {message}")).ok();
                    }
                    ProgressEvent::Error { message, .. } => {
                        multi.println(format!("  [error] {message}")).ok();
                    }
                    ProgressEvent::TranslationFinished {
                        succeeded,
                        cached: c,
                        needs_review,
                        failed,
                        ..
                    } => {
                        seg_bar.set_position(*succeeded as u64 + *c as u64);
                        seg_bar.finish_with_message(format!(
                            "{} done, {} needs review, {} failed",
                            succeeded + c,
                            needs_review,
                            failed
                        ));
                        stage_bar.finish_and_clear();
                        batch_bar.finish_and_clear();
                        rate_bar.finish_and_clear();
                        checkpoint_bar.finish_and_clear();
                    }
                    _ => {}
                }
            }
            Ok(None) => break, // Channel closed
            Err(_) => {}       // Timeout, just re-render
        }

        // Throttle rendering to ~250ms
        if last_render.elapsed() >= std::time::Duration::from_millis(250) {
            let dropped_count = dropped.load(Ordering::Relaxed);
            if dropped_count > 0 {
                multi
                    .println(format!("  ({} progress events dropped)", dropped_count))
                    .ok();
            }
            last_render = Instant::now();
        }

        // Open JSONL file lazily on first event; skip if we already tried and failed.
        if jsonl_writer.is_none()
            && !jsonl_failed
            && let Some(ref path) = jsonl_path
        {
            match std::fs::File::create(path) {
                Ok(f) => {
                    jsonl_writer = Some(BufWriter::new(f));
                    jsonl_last_flush = Instant::now();
                }
                Err(e) => {
                    jsonl_failed = true;
                    let _ = multi.println(format!("  [warn] cannot create JSONL log: {e}"));
                }
            }
        }
    }

    multi.clear().ok();
    Ok(())
}

async fn render_jsonl_stdout(rx: &mut mpsc::Receiver<ProgressEvent>) -> Result<()> {
    while let Some(event) = rx.recv().await {
        let line = serde_json::to_string(&event).unwrap_or_default();
        println!("{line}");
    }
    Ok(())
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
