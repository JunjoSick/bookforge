use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::Result;
use bookforge_core::{
    ControlCommand, ProgressEvent, ProgressSink, clear_control_file, control_path_for_job, now_ms,
    read_control_file, write_control_file,
};
use bookforge_llm::{PauseSignal, PauseState};
use bookforge_store::JobStore;
use tokio_util::sync::CancellationToken;

const CONTROL_POLL_INTERVAL: Duration = Duration::from_millis(100);

pub(crate) fn request_job_control(job_id: &str, command: ControlCommand) -> Result<PathBuf> {
    let path = control_path_for_job(job_id);
    write_control_file(&path, command)?;
    Ok(path)
}

pub(crate) fn clear_job_control(job_id: &str) -> Result<PathBuf> {
    let path = control_path_for_job(job_id);
    clear_control_file(&path)?;
    Ok(path)
}

pub(crate) struct ControlFilePoller<'a> {
    store: &'a JobStore,
    job_id: String,
    path: PathBuf,
    progress: Arc<dyn ProgressSink>,
    last_state: PauseState,
    stop_cancel_token: Option<CancellationToken>,
}

impl<'a> ControlFilePoller<'a> {
    pub(crate) fn new(
        store: &'a JobStore,
        job_id: impl Into<String>,
        progress: Arc<dyn ProgressSink>,
    ) -> Self {
        Self::new_inner(store, job_id, control_path_for_job, progress, None)
    }

    pub(crate) fn new_with_stop_cancel(
        store: &'a JobStore,
        job_id: impl Into<String>,
        progress: Arc<dyn ProgressSink>,
        stop_cancel_token: CancellationToken,
    ) -> Self {
        Self::new_inner(
            store,
            job_id,
            control_path_for_job,
            progress,
            Some(stop_cancel_token),
        )
    }

    fn new_inner(
        store: &'a JobStore,
        job_id: impl Into<String>,
        path_for_job: impl FnOnce(&str) -> PathBuf,
        progress: Arc<dyn ProgressSink>,
        stop_cancel_token: Option<CancellationToken>,
    ) -> Self {
        let job_id = job_id.into();
        Self {
            path: path_for_job(&job_id),
            store,
            job_id,
            progress,
            last_state: PauseState::Running,
            stop_cancel_token,
        }
    }

    #[cfg(test)]
    pub(crate) fn new_with_path(
        store: &'a JobStore,
        job_id: impl Into<String>,
        path: PathBuf,
        progress: Arc<dyn ProgressSink>,
    ) -> Self {
        Self {
            store,
            job_id: job_id.into(),
            path,
            progress,
            last_state: PauseState::Running,
            stop_cancel_token: None,
        }
    }

    pub(crate) fn poll(&mut self, signal: &PauseSignal) -> Result<()> {
        match read_control_file(&self.path)? {
            ControlCommand::Pause => self.pause(signal),
            ControlCommand::Resume | ControlCommand::Run => self.resume(signal),
            ControlCommand::Stop => self.stop(signal),
        }
    }

    pub(crate) async fn wait_until_running_or_stopped(
        &mut self,
        signal: &PauseSignal,
    ) -> Result<PauseState> {
        loop {
            self.poll(signal)?;
            match signal.state() {
                PauseState::Running => return Ok(PauseState::Running),
                PauseState::Stopped => return Ok(PauseState::Stopped),
                PauseState::Paused => tokio::time::sleep(CONTROL_POLL_INTERVAL).await,
            }
        }
    }

    fn pause(&mut self, signal: &PauseSignal) -> Result<()> {
        if signal.state() == PauseState::Stopped || self.job_status_is("stopped")? {
            signal.stop();
            self.last_state = PauseState::Stopped;
            return Ok(());
        }
        if signal.pause() {
            self.store.mark_job_paused(&self.job_id)?;
            if self.job_status_is("paused")? && self.last_state != PauseState::Paused {
                self.progress.emit(ProgressEvent::JobPaused {
                    job_id: self.job_id.clone(),
                    timestamp_ms: now_ms(),
                });
                self.last_state = PauseState::Paused;
            }
        }
        Ok(())
    }

    fn resume(&mut self, signal: &PauseSignal) -> Result<()> {
        if signal.state() == PauseState::Stopped || self.job_status_is("stopped")? {
            signal.stop();
            self.last_state = PauseState::Stopped;
            return Ok(());
        }
        if !signal.resume() {
            self.last_state = signal.state();
            return Ok(());
        }
        self.store.mark_job_running(&self.job_id)?;
        if self.job_status_is("running")? {
            self.progress.emit(ProgressEvent::JobResumed {
                job_id: self.job_id.clone(),
                timestamp_ms: now_ms(),
            });
            self.last_state = PauseState::Running;
        }
        Ok(())
    }

    fn stop(&mut self, signal: &PauseSignal) -> Result<()> {
        if let Some(token) = &self.stop_cancel_token {
            token.cancel();
        }
        if signal.stop() {
            self.store.mark_job_stopped(&self.job_id)?;
            self.last_state = PauseState::Stopped;
        }
        Ok(())
    }

    fn job_status_is(&self, expected: &str) -> Result<bool> {
        Ok(self
            .store
            .get_job(&self.job_id)?
            .is_some_and(|job| job.status == expected))
    }
}

pub(crate) struct ControlFileWatcher {
    cancel: CancellationToken,
    handle: tokio::task::JoinHandle<()>,
}

impl ControlFileWatcher {
    pub(crate) fn spawn_with_stop_cancel(
        store_path: PathBuf,
        job_id: impl Into<String>,
        progress: Arc<dyn ProgressSink>,
        signal: PauseSignal,
        stop_cancel_token: CancellationToken,
    ) -> Self {
        Self::spawn_inner(
            store_path,
            job_id,
            progress,
            signal,
            Some(stop_cancel_token),
        )
    }

    fn spawn_inner(
        store_path: PathBuf,
        job_id: impl Into<String>,
        progress: Arc<dyn ProgressSink>,
        signal: PauseSignal,
        stop_cancel_token: Option<CancellationToken>,
    ) -> Self {
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let job_id = job_id.into();
        let handle = tokio::spawn(async move {
            loop {
                {
                    match JobStore::open(store_path.clone()) {
                        Ok(store) => {
                            let mut poller = ControlFilePoller::new_inner(
                                &store,
                                job_id.clone(),
                                control_path_for_job,
                                progress.clone(),
                                stop_cancel_token.clone(),
                            );
                            if let Err(error) = poller.poll(&signal) {
                                progress.emit(ProgressEvent::Error {
                                    kind: "control_file_watcher".to_string(),
                                    message: format!("failed to poll control file: {error}"),
                                    timestamp_ms: now_ms(),
                                });
                            }
                        }
                        Err(error) => {
                            progress.emit(ProgressEvent::Error {
                                kind: "control_file_watcher".to_string(),
                                message: format!(
                                    "failed to open job store for control watcher: {error}"
                                ),
                                timestamp_ms: now_ms(),
                            });
                        }
                    }
                }
                tokio::select! {
                    _ = task_cancel.cancelled() => break,
                    _ = tokio::time::sleep(CONTROL_POLL_INTERVAL) => {}
                }
            }
        });
        Self { cancel, handle }
    }

    #[allow(dead_code)]
    pub(crate) async fn shutdown(self) {
        self.cancel.cancel();
    }
}

impl Drop for ControlFileWatcher {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.handle.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookforge_core::NullProgressSink;

    #[test]
    fn request_and_clear_job_control_use_conventional_path() {
        let job_id = format!(
            "job_control_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let path = request_job_control(&job_id, ControlCommand::Pause).unwrap();
        assert_eq!(path, control_path_for_job(&job_id));
        assert_eq!(read_control_file(&path).unwrap(), ControlCommand::Pause);

        clear_job_control(&job_id).unwrap();
        assert_eq!(read_control_file(&path).unwrap(), ControlCommand::Run);
        let _ = std::fs::remove_dir_all(bookforge_core::run_dir_for_job(&job_id));
    }

    #[test]
    fn poller_treats_missing_and_garbage_control_as_running() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("jobs.sqlite");
        let input = dir.path().join("input.epub");
        std::fs::write(&input, b"epub").unwrap();
        let store = JobStore::open(&db).unwrap();
        let job = store
            .create_job(bookforge_store::CreateJob {
                input: &input,
                output: &dir.path().join("out.epub"),
                source_lang: Some("English"),
                target_lang: "Italian",
                provider: "mock",
                model: "mock-prefix",
                base_url: None,
                api_key_env: None,
                book_id: None,
                series_id: None,
            })
            .unwrap();
        store.mark_job_paused(&job.id).unwrap();

        let signal = PauseSignal::new();
        signal.pause();
        let control_path = dir.path().join("control");
        let mut poller = ControlFilePoller::new_with_path(
            &store,
            &job.id,
            control_path.clone(),
            Arc::new(NullProgressSink),
        );
        poller.poll(&signal).unwrap();
        assert_eq!(signal.state(), PauseState::Running);
        assert_eq!(store.get_job(&job.id).unwrap().unwrap().status, "running");

        std::fs::write(&control_path, "garbage").unwrap();
        signal.pause();
        poller.poll(&signal).unwrap();
        assert_eq!(signal.state(), PauseState::Running);
    }
}
