//! `bookforge watch` — a live terminal dashboard for a translation job.
//!
//! Unlike `translate --ui tui` (which attaches to a run in this process), this
//! follows a job that may be running in another process — or already finished —
//! by tailing its `events.jsonl` log and folding it into the shared
//! [`bookforge_core::RunState`]. With no job id it shows a picker over recent
//! jobs. Pressing `r` marks failed/needs-review segments for retry.

use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Result;
use bookforge_core::ProgressEvent;
use bookforge_store::{JobRecord, JobStore, RetryScope};

use crate::tui::{JobPickerEntry, TuiAction, TuiApp, TuiMode, pick_job};

#[derive(Debug, clap::Args)]
pub struct WatchArgs {
    /// Job id to watch. Omit to pick from a list of recent jobs.
    pub job_id: Option<String>,

    /// Refresh interval in milliseconds.
    #[arg(long, default_value_t = 150)]
    pub refresh_ms: u64,
}

pub async fn run(args: WatchArgs) -> Result<()> {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        anyhow::bail!(
            "`bookforge watch` needs an interactive terminal; run it directly in a TTY \
             (use `bookforge status <job>` or `bookforge tail <job>` for non-interactive output)"
        );
    }

    let store = JobStore::open_default()?;

    let job_id = match args.job_id {
        Some(id) => id,
        None => {
            let entries = job_picker_entries(&store)?;
            if entries.is_empty() {
                println!("No jobs found in {}.", store.path().display());
                return Ok(());
            }
            match pick_job(entries)? {
                Some(id) => id,
                None => return Ok(()),
            }
        }
    };

    let job = store.get_job(&job_id)?;
    let events_path = events_path_for(job.as_ref(), &job_id);
    if job.is_none() && !events_path.exists() {
        anyhow::bail!(
            "no job '{job_id}' in {} and no event log at {}",
            store.path().display(),
            events_path.display()
        );
    }

    let refresh = Duration::from_millis(args.refresh_ms.clamp(20, 5_000));
    watch_job(&store, &job_id, &events_path, refresh).await
}

fn job_picker_entries(store: &JobStore) -> Result<Vec<JobPickerEntry>> {
    Ok(store
        .list_job_summaries()?
        .into_iter()
        .map(|(job, summary)| {
            let done = summary.succeeded + summary.cached + summary.needs_review + summary.failed;
            JobPickerEntry {
                line: format!(
                    "{:<26} {:<13} {:>4}/{:<4}  ⚠{:<3} ✗{:<3}  {}/{}",
                    job.id,
                    summary.status,
                    done,
                    summary.total_segments,
                    summary.needs_review,
                    summary.failed,
                    job.provider,
                    job.model,
                ),
                id: job.id,
            }
        })
        .collect())
}

fn events_path_for(job: Option<&JobRecord>, job_id: &str) -> PathBuf {
    job.and_then(|j| j.events_path.clone())
        .unwrap_or_else(|| PathBuf::from(format!(".bookforge/runs/{job_id}/events.jsonl")))
}

async fn watch_job(store: &JobStore, job_id: &str, path: &Path, refresh: Duration) -> Result<()> {
    let mut app = TuiApp::new(TuiMode::Watch)?;
    let result = drive_watch(&mut app, store, job_id, path, refresh).await;
    let restore = app.restore();
    result.and(restore)
}

async fn drive_watch(
    app: &mut TuiApp,
    store: &JobStore,
    job_id: &str,
    path: &Path,
    refresh: Duration,
) -> Result<()> {
    let mut file: Option<File> = None;
    let mut buf: Vec<u8> = Vec::new();
    let mut tick = tokio::time::interval(refresh);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Show the current state immediately, before waiting on the first tick.
    pump_file(&mut file, path, &mut buf, app)?;
    app.draw()?;

    loop {
        tick.tick().await;
        pump_file(&mut file, path, &mut buf, app)?;
        if app.pump_input()? {
            break;
        }
        for action in app.take_actions() {
            match action {
                TuiAction::RetryFlagged => match store.retry_segments(job_id, RetryScope::All) {
                    Ok(0) => app.set_status("no failed/needs-review segments to retry".to_string()),
                    Ok(n) => app.set_status(format!(
                        "marked {n} segment(s) for retry — run: bookforge resume {job_id}"
                    )),
                    Err(err) => app.set_status(format!("retry failed: {err}")),
                },
            }
        }
        app.draw()?;
    }
    Ok(())
}

/// Read newly-appended bytes and fold each complete JSONL event into the app.
/// Tolerates a partial trailing line (kept in `buf` until the newline arrives)
/// and a not-yet-created log file (retried on the next tick).
fn pump_file(
    file: &mut Option<File>,
    path: &Path,
    buf: &mut Vec<u8>,
    app: &mut TuiApp,
) -> Result<()> {
    if file.is_none() {
        match File::open(path) {
            Ok(opened) => *file = Some(opened),
            Err(_) => return Ok(()),
        }
    }
    let Some(handle) = file.as_mut() else {
        return Ok(());
    };

    let mut chunk = Vec::new();
    handle.read_to_end(&mut chunk)?;
    if chunk.is_empty() {
        return Ok(());
    }
    buf.extend_from_slice(&chunk);

    let mut start = 0;
    while let Some(offset) = buf[start..].iter().position(|&b| b == b'\n') {
        let end = start + offset;
        let line = &buf[start..end];
        if !line.is_empty()
            && let Ok(event) = serde_json::from_slice::<ProgressEvent>(line)
        {
            app.fold(&event);
        }
        start = end + 1;
    }
    buf.drain(..start);
    Ok(())
}
