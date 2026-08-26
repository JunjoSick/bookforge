use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::Result;
use bookforge_core::{
    ControlCommand, ProgressEvent, ProgressSink, ResolvedRunSettings, clear_control_file,
    control_path_for_job, now_ms, read_control_file, write_control_file,
};
use bookforge_llm::{EngineRuntimeSettings, PauseSignal, PauseState, TranslationRunConfig};
use bookforge_store::JobStore;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::QaMode;

const CONTROL_POLL_INTERVAL: Duration = Duration::from_millis(100);
pub(crate) const RUNTIME_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
pub(crate) const RUNTIME_LEASE_STALE_AFTER: Duration = Duration::from_secs(3);
const RUNTIME_LAUNCH_CLAIM_STALE_AFTER: Duration = Duration::from_secs(10);
static RUNTIME_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimeLease {
    pub schema_version: u32,
    pub instance_id: String,
    pub pid: u32,
    pub process_started_at_ms: u64,
    pub heartbeat_at_ms: u64,
    pub last_loaded_revision: u64,
    pub last_applied_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RuntimeLeaseState {
    Missing,
    Fresh(RuntimeLease),
    Stale(RuntimeLease),
    Invalid(String),
}

pub(crate) fn runtime_path_for_job(job_id: &str) -> PathBuf {
    bookforge_core::run_dir_for_job(job_id).join("runtime.json")
}

pub(crate) fn runtime_lease_state(job_id: &str, stale_after: Duration) -> RuntimeLeaseState {
    runtime_lease_state_at(job_id, stale_after, now_ms())
}

fn runtime_lease_state_at(
    job_id: &str,
    stale_after: Duration,
    observed_at_ms: u64,
) -> RuntimeLeaseState {
    let path = runtime_path_for_job(job_id);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return RuntimeLeaseState::Missing;
        }
        Err(error) => return RuntimeLeaseState::Invalid(error.to_string()),
    };
    let lease = match serde_json::from_str::<RuntimeLease>(&contents) {
        Ok(lease) if lease.schema_version == 1 => lease,
        Ok(lease) => {
            return RuntimeLeaseState::Invalid(format!(
                "unsupported runtime lease schema {}",
                lease.schema_version
            ));
        }
        Err(error) => return RuntimeLeaseState::Invalid(error.to_string()),
    };
    let age_ms = observed_at_ms.saturating_sub(lease.heartbeat_at_ms);
    let stale_after_ms = u64::try_from(stale_after.as_millis()).unwrap_or(u64::MAX);
    if age_ms <= stale_after_ms {
        RuntimeLeaseState::Fresh(lease)
    } else {
        RuntimeLeaseState::Stale(lease)
    }
}

fn write_runtime_lease(path: &Path, lease: &RuntimeLease) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let suffix = RUNTIME_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let staged = path.with_file_name(format!(
        ".runtime.json.staged-{}-{suffix}",
        std::process::id()
    ));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&staged)?;
        let json = serde_json::to_string_pretty(lease)?;
        file.write_all(format!("{json}\n").as_bytes())?;
        file.sync_all()?;
        fs::rename(&staged, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    result
}

fn remove_runtime_lease_if_owned(path: &Path, instance_id: &str) {
    let owned = fs::read_to_string(path)
        .ok()
        .and_then(|contents| serde_json::from_str::<RuntimeLease>(&contents).ok())
        .is_some_and(|lease| lease.instance_id == instance_id);
    if owned {
        let _ = fs::remove_file(path);
    }
}

pub(crate) struct RuntimeLaunchClaim {
    path: PathBuf,
    remove_on_drop: bool,
}

impl RuntimeLaunchClaim {
    pub(crate) fn acquire(job_id: &str) -> Result<Option<Self>> {
        Self::acquire_with_stale_after(job_id, RUNTIME_LAUNCH_CLAIM_STALE_AFTER)
    }

    fn acquire_with_stale_after(job_id: &str, stale_after: Duration) -> Result<Option<Self>> {
        let path = bookforge_core::run_dir_for_job(job_id).join("resume.launch");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        for _ in 0..2 {
            match OpenOptions::new().create_new(true).write(true).open(&path) {
                Ok(mut file) => {
                    writeln!(file, "{} {}", std::process::id(), now_ms())?;
                    file.sync_all()?;
                    return Ok(Some(Self {
                        path,
                        remove_on_drop: true,
                    }));
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = fs::metadata(&path)
                        .and_then(|metadata| metadata.modified())
                        .ok()
                        .and_then(|modified| modified.elapsed().ok())
                        .is_some_and(|age| age >= stale_after);
                    if !stale {
                        return Ok(None);
                    }
                    // Reclaim via rename instead of check-then-delete: exactly
                    // one racer wins the rename, so a concurrent acquirer can
                    // never lose a freshly created claim between our staleness
                    // check and an unlink. Whoever loses observes either
                    // NotFound (the other racer moved it first) or a replaced
                    // file and simply backs off.
                    let reclaimed = path.with_file_name(format!(
                        ".resume.launch.reclaimed-{}-{}",
                        std::process::id(),
                        RUNTIME_FILE_COUNTER.fetch_add(1, Ordering::Relaxed)
                    ));
                    match fs::rename(&path, &reclaimed) {
                        Ok(()) => {
                            let _ = fs::remove_file(&reclaimed);
                            continue;
                        }
                        Err(_) => return Ok(None),
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(None)
    }

    pub(crate) fn persist_until_worker(&mut self) {
        self.remove_on_drop = false;
    }
}

impl Drop for RuntimeLaunchClaim {
    fn drop(&mut self) {
        if self.remove_on_drop {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct JobRuntimeSettings {
    pub revision: u64,
    pub settings: ResolvedRunSettings,
    pub qa: QaMode,
    pub validate_output: bool,
}

pub(crate) fn freeze_run_config_for_stage(
    base: &TranslationRunConfig,
    runtime: &JobRuntimeSettings,
) -> TranslationRunConfig {
    let mut frozen = base.clone();
    frozen.scheduler.concurrency = runtime.settings.scheduler.concurrency.max(1);
    frozen.batch_max_output_tokens = runtime.settings.provider.batch_max_output_tokens;
    let (_sender, receiver) = watch::channel(EngineRuntimeSettings::from_resolved(
        runtime.revision,
        &runtime.settings,
    ));
    frozen.runtime_settings = Some(receiver);
    frozen
}

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
        if self.job_outcome_is_final()? {
            // The job already reached a completion outcome (succeeded,
            // needs_review, or failed). A pause landing in the post-completion
            // window must not rewrite that outcome.
            return Ok(());
        }
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
        if self.job_outcome_is_final()? {
            // Completion is final: a stop that lands after the job recorded
            // its outcome must not flip it back to stopped.
            return Ok(());
        }
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

    /// True once the job row records a completion outcome that late pause/stop
    /// commands must not rewrite (CLI-4). Work-in-progress statuses ("running",
    /// "paused", "stopped", ...) keep the old control semantics.
    fn job_outcome_is_final(&self) -> Result<bool> {
        Ok(self.store.get_job(&self.job_id)?.is_some_and(|job| {
            matches!(job.status.as_str(), "succeeded" | "needs_review" | "failed")
        }))
    }
}

pub(crate) struct ControlFileWatcher {
    cancel: CancellationToken,
    handle: std::thread::JoinHandle<()>,
    runtime_settings: watch::Receiver<EngineRuntimeSettings>,
    job_runtime_settings: watch::Receiver<JobRuntimeSettings>,
    lease_path: PathBuf,
    lease_instance_id: String,
    #[cfg(test)]
    heartbeat_updates: watch::Receiver<u64>,
}

pub(crate) struct ControlBaseline {
    pub settings: ResolvedRunSettings,
    pub qa: QaMode,
    pub validate_output: bool,
}

impl ControlFileWatcher {
    pub(crate) fn spawn_with_stop_cancel(
        store_path: PathBuf,
        job_id: impl Into<String>,
        progress: Arc<dyn ProgressSink>,
        signal: PauseSignal,
        stop_cancel_token: CancellationToken,
        baseline: ControlBaseline,
    ) -> Self {
        Self::spawn_inner(
            store_path,
            job_id,
            progress,
            signal,
            Some(stop_cancel_token),
            baseline,
        )
    }

    fn spawn_inner(
        store_path: PathBuf,
        job_id: impl Into<String>,
        progress: Arc<dyn ProgressSink>,
        signal: PauseSignal,
        stop_cancel_token: Option<CancellationToken>,
        baseline: ControlBaseline,
    ) -> Self {
        let ControlBaseline {
            settings: baseline_settings,
            qa: baseline_qa,
            validate_output: baseline_validate_output,
        } = baseline;
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let job_id = job_id.into();
        // A resumed process may already have a durable sidecar. Load it before
        // returning the receivers so the very first dispatch cannot race the
        // watcher's asynchronous poll and be mislabeled as revision zero.
        let initial_loaded = crate::commands::reconfigure::load_overrides_document_for_job(&job_id)
            .ok()
            .flatten();
        let initial_revision = initial_loaded.as_ref().map_or(0, |loaded| loaded.revision);
        let mut initial_settings = baseline_settings.clone();
        let mut initial_qa = baseline_qa;
        let mut initial_validate_output = baseline_validate_output;
        if let Some(loaded) = initial_loaded.as_ref() {
            crate::commands::reconfigure::apply_overrides_to_settings(
                &mut initial_settings,
                &loaded.overrides,
            );
            initial_qa = loaded.overrides.qa.unwrap_or(baseline_qa);
            initial_validate_output = loaded
                .overrides
                .validate_output
                .unwrap_or(baseline_validate_output);
        }
        let (runtime_sender, runtime_settings) = watch::channel(
            EngineRuntimeSettings::from_resolved(initial_revision, &initial_settings),
        );
        let (job_runtime_sender, job_runtime_settings) = watch::channel(JobRuntimeSettings {
            revision: initial_revision,
            settings: initial_settings,
            qa: initial_qa,
            validate_output: initial_validate_output,
        });
        let process_started_at_ms = now_ms();
        #[cfg(test)]
        let (heartbeat_sender, heartbeat_updates) = watch::channel(process_started_at_ms);
        let lease_path = runtime_path_for_job(&job_id);
        let lease_instance_id = format!(
            "{}-{process_started_at_ms}-{}",
            std::process::id(),
            RUNTIME_FILE_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let mut lease = RuntimeLease {
            schema_version: 1,
            instance_id: lease_instance_id.clone(),
            pid: std::process::id(),
            process_started_at_ms,
            heartbeat_at_ms: process_started_at_ms,
            last_loaded_revision: initial_revision,
            last_applied_revision: initial_revision,
        };
        if let Err(error) = write_runtime_lease(&lease_path, &lease) {
            progress.emit(ProgressEvent::Error {
                kind: "runtime_lease".to_string(),
                message: format!("failed to create runtime lease: {error}"),
                timestamp_ms: now_ms(),
            });
        }
        if let Some(loaded) = initial_loaded.as_ref() {
            progress.emit(ProgressEvent::RuntimeConfigChanged {
                revision: loaded.revision,
                changed_fields: loaded.overrides.changed_fields(),
                application: loaded.overrides.application_boundaries(),
                timestamp_ms: now_ms(),
            });
        }
        let _ = fs::remove_file(bookforge_core::run_dir_for_job(&job_id).join("resume.launch"));
        let task_lease_path = lease_path.clone();
        let task_lease_instance_id = lease_instance_id.clone();
        // JobStore holds a RefCell'd connection and is therefore !Send, so the
        // polling loop runs on a dedicated blocking thread that OWNS one
        // long-lived watch store. Opening + migrating the database once here —
        // instead of every 100 ms tick — removes the per-run SQLite churn tax
        // on the checkpoint writer (H-7); transient failures drop the store
        // and reopen on the next tick.
        let handle = std::thread::Builder::new()
            .name(format!("bookforge-control-{job_id}"))
            .spawn(move || {
                let mut last_override_revision =
                    initial_loaded.as_ref().map(|loaded| loaded.revision);
                let mut last_override_error = None;
                let mut last_store_error: Option<String> = None;
                let mut last_heartbeat_write = Instant::now();
                let mut watch_store = match JobStore::open(store_path.clone()) {
                    Ok(store) => Some(store),
                    Err(error) => {
                        last_store_error = Some(error.to_string());
                        None
                    }
                };
                loop {
                    match crate::commands::reconfigure::load_overrides_document_for_job(&job_id) {
                        Ok(Some(loaded)) if last_override_revision != Some(loaded.revision) => {
                            let mut effective = baseline_settings.clone();
                            crate::commands::reconfigure::apply_overrides_to_settings(
                                &mut effective,
                                &loaded.overrides,
                            );
                            let changed_fields = loaded.overrides.changed_fields();
                            let effective_qa = loaded.overrides.qa.unwrap_or(baseline_qa);
                            let effective_validate_output = loaded
                                .overrides
                                .validate_output
                                .unwrap_or(baseline_validate_output);
                            job_runtime_sender.send_replace(JobRuntimeSettings {
                                revision: loaded.revision,
                                settings: effective.clone(),
                                qa: effective_qa,
                                validate_output: effective_validate_output,
                            });
                            runtime_sender.send_replace(EngineRuntimeSettings::from_resolved(
                                loaded.revision,
                                &effective,
                            ));
                            lease.last_loaded_revision = loaded.revision;
                            lease.last_applied_revision = loaded.revision;
                            progress.emit(ProgressEvent::RuntimeConfigChanged {
                                revision: loaded.revision,
                                changed_fields,
                                application: loaded.overrides.application_boundaries(),
                                timestamp_ms: now_ms(),
                            });
                            last_override_revision = Some(loaded.revision);
                            last_override_error = None;
                        }
                        Ok(_) => {}
                        Err(error) => {
                            let message = error.to_string();
                            if last_override_error.as_deref() != Some(message.as_str()) {
                                progress.emit(ProgressEvent::RuntimeConfigRejected {
                                    revision: None,
                                    message: message.clone(),
                                    timestamp_ms: now_ms(),
                                });
                                last_override_error = Some(message);
                            }
                        }
                    }
                    if let Some(store) = watch_store.as_ref() {
                        let mut poller = ControlFilePoller::new_inner(
                            store,
                            job_id.clone(),
                            control_path_for_job,
                            progress.clone(),
                            stop_cancel_token.clone(),
                        );
                        // Publish a newly durable override revision before a Resume
                        // command can release paused dispatchers. This preserves the
                        // reconfigure-then-resume ordering guarantee across processes.
                        match poller.poll(&signal) {
                            Ok(()) => last_store_error = None,
                            Err(error) => {
                                // Reopen-on-error: a corrupted or externally
                                // closed connection must not wedge the watcher
                                // forever; the next tick gets a fresh store.
                                let message = format!("failed to poll control file: {error}");
                                if last_store_error.as_deref() != Some(message.as_str()) {
                                    progress.emit(ProgressEvent::Error {
                                        kind: "control_file_watcher".to_string(),
                                        message: message.clone(),
                                        timestamp_ms: now_ms(),
                                    });
                                    tracing::warn!(
                                        job_id = %job_id,
                                        "{message}; reopening the watch store"
                                    );
                                }
                                last_store_error = Some(message);
                            }
                        }
                        if last_store_error.is_some() {
                            watch_store = None;
                        }
                    } else {
                        match JobStore::open(store_path.clone()) {
                            Ok(store) => {
                                last_store_error = None;
                                watch_store = Some(store);
                            }
                            Err(error) => {
                                let message = format!(
                                    "failed to open job store for control watcher: {error}"
                                );
                                if last_store_error.as_deref() != Some(message.as_str()) {
                                    progress.emit(ProgressEvent::Error {
                                        kind: "control_file_watcher".to_string(),
                                        message: message.clone(),
                                        timestamp_ms: now_ms(),
                                    });
                                }
                                last_store_error = Some(message);
                            }
                        }
                    }
                    if last_heartbeat_write.elapsed() >= RUNTIME_HEARTBEAT_INTERVAL {
                        lease.heartbeat_at_ms = now_ms();
                        match write_runtime_lease(&task_lease_path, &lease) {
                            Ok(()) => {
                                #[cfg(test)]
                                heartbeat_sender.send_replace(lease.heartbeat_at_ms);
                            }
                            Err(error) => {
                                progress.emit(ProgressEvent::Error {
                                    kind: "runtime_lease".to_string(),
                                    message: format!("failed to refresh runtime lease: {error}"),
                                    timestamp_ms: now_ms(),
                                });
                            }
                        }
                        last_heartbeat_write = Instant::now();
                    }
                    if task_cancel.is_cancelled() {
                        break;
                    }
                    std::thread::sleep(CONTROL_POLL_INTERVAL);
                }
                remove_runtime_lease_if_owned(&task_lease_path, &task_lease_instance_id);
            })
            .expect("control watcher thread should spawn");
        Self {
            cancel,
            handle,
            runtime_settings,
            job_runtime_settings,
            lease_path,
            lease_instance_id,
            #[cfg(test)]
            heartbeat_updates,
        }
    }

    pub(crate) fn runtime_settings(&self) -> watch::Receiver<EngineRuntimeSettings> {
        self.runtime_settings.clone()
    }

    pub(crate) fn job_runtime_settings(&self) -> watch::Receiver<JobRuntimeSettings> {
        self.job_runtime_settings.clone()
    }

    #[cfg(test)]
    fn heartbeat_updates(&self) -> watch::Receiver<u64> {
        self.heartbeat_updates.clone()
    }
}

impl Drop for ControlFileWatcher {
    fn drop(&mut self) {
        // The poll thread observes cancellation within one CONTROL_POLL_INTERVAL
        // and then removes the lease itself. Joining here would block on a
        // synchronous SQLite tick, so drop only requests the exit (the
        // heartbeat-removal test asserts this converges). Reading the handle
        // keeps the field considered used for the join-based shutdown path.
        self.cancel.cancel();
        let _ = &self.handle;
        remove_runtime_lease_if_owned(&self.lease_path, &self.lease_instance_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookforge_core::{NullProgressSink, TranslationProfile};

    // These waits span synchronous SQLite opens and fsync-heavy lease writes
    // on a current-thread runtime, which starve badly when the whole suite
    // runs on a saturated box. The guard exists to catch deadlocks, not load
    // spikes, so the deadline stays far above any legitimate scheduling delay.
    const TEST_DEADLOCK_TIMEOUT: Duration = Duration::from_secs(120);

    struct RecordingSink {
        events: tokio::sync::mpsc::UnboundedSender<ProgressEvent>,
    }

    impl ProgressSink for RecordingSink {
        fn emit(&self, event: ProgressEvent) {
            let _ = self.events.send(event);
        }
    }

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

    #[tokio::test]
    async fn watcher_publishes_revisioned_runtime_overrides() {
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
        let run_dir = bookforge_core::run_dir_for_job(&job.id);
        std::fs::create_dir_all(&run_dir).unwrap();
        let overrides_path = run_dir.join("overrides.json");
        std::fs::write(
            &overrides_path,
            r#"{
  "schema_version": 1,
  "revision": 7,
  "updated_at_ms": 123,
  "overrides": {
    "concurrency": 2,
    "batch_max_output_tokens": 12000,
    "qa": "all",
    "validate_output": true
  }
}"#,
        )
        .unwrap();

        let baseline = TranslationProfile::V1Fast.resolve();
        let watcher = ControlFileWatcher::spawn_with_stop_cancel(
            db,
            job.id.clone(),
            Arc::new(NullProgressSink),
            PauseSignal::new(),
            CancellationToken::new(),
            ControlBaseline {
                settings: baseline,
                qa: QaMode::Off,
                validate_output: false,
            },
        );
        let mut receiver = watcher.runtime_settings();
        if receiver.borrow().revision != 7 {
            tokio::time::timeout(TEST_DEADLOCK_TIMEOUT, receiver.changed())
                .await
                .expect("watcher should publish the sidecar")
                .expect("runtime channel should stay open");
        }
        let applied = receiver.borrow().clone();
        assert_eq!(applied.revision, 7);
        assert_eq!(applied.concurrency, 2);
        assert_eq!(applied.batch_max_output_tokens, Some(12_000));
        let job_runtime = watcher.job_runtime_settings();
        let job_runtime = job_runtime.borrow();
        assert_eq!(job_runtime.revision, 7);
        assert_eq!(job_runtime.qa, QaMode::All);
        assert!(job_runtime.validate_output);

        drop(watcher);
        let _ = std::fs::remove_dir_all(run_dir);
    }

    #[tokio::test]
    async fn watcher_runtime_lease_heartbeats_and_is_removed_on_drop() {
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
        let run_dir = bookforge_core::run_dir_for_job(&job.id);
        let watcher = ControlFileWatcher::spawn_with_stop_cancel(
            db,
            job.id.clone(),
            Arc::new(NullProgressSink),
            PauseSignal::new(),
            CancellationToken::new(),
            ControlBaseline {
                settings: TranslationProfile::V1Fast.resolve(),
                qa: QaMode::Off,
                validate_output: false,
            },
        );

        let mut heartbeat_updates = watcher.heartbeat_updates();
        let first = match runtime_lease_state(&job.id, Duration::from_millis(u64::MAX)) {
            RuntimeLeaseState::Fresh(lease) => lease,
            state => panic!("expected a fresh runtime lease, got {state:?}"),
        };
        // The watcher only refreshes the lease after RUNTIME_HEARTBEAT_INTERVAL
        // and swallows transient write failures (it retries on the next tick),
        // so wait for the first observed heartbeat inside the deadlock guard
        // instead of hanging forever if writes keep failing.
        tokio::time::timeout(TEST_DEADLOCK_TIMEOUT, async {
            loop {
                if *heartbeat_updates.borrow_and_update() > first.heartbeat_at_ms {
                    break;
                }
                heartbeat_updates
                    .changed()
                    .await
                    .expect("watcher should report a newer successful heartbeat write");
            }
        })
        .await
        .expect("watcher should publish a heartbeat within the deadlock guard");
        let refreshed = match runtime_lease_state(&job.id, Duration::from_millis(u64::MAX)) {
            RuntimeLeaseState::Fresh(lease) => lease,
            state => panic!("expected a refreshed runtime lease, got {state:?}"),
        };
        assert_eq!(refreshed.instance_id, first.instance_id);
        assert!(refreshed.heartbeat_at_ms > first.heartbeat_at_ms);

        drop(watcher);
        // Drop cancels and aborts the task, but abort cannot interrupt a
        // synchronous iteration already in progress: under load the task can
        // still be inside a lease write and perform one final write after
        // Drop's own removal, before its exit path removes the file again.
        // Removal-on-drop therefore converges rather than being instantaneous,
        // so poll briefly instead of asserting immediately.
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if matches!(
                    runtime_lease_state(&job.id, RUNTIME_LEASE_STALE_AFTER),
                    RuntimeLeaseState::Missing
                ) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("runtime lease should be removed when the watcher drops");
        let _ = std::fs::remove_dir_all(run_dir);
    }

    #[tokio::test]
    async fn watcher_publishes_overrides_before_applying_resume() {
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

        let run_dir = bookforge_core::run_dir_for_job(&job.id);
        std::fs::create_dir_all(&run_dir).unwrap();
        std::fs::write(
            run_dir.join("overrides.json"),
            r#"{
  "schema_version": 1,
  "revision": 9,
  "updated_at_ms": 123,
  "overrides": { "concurrency": 2 }
}"#,
        )
        .unwrap();
        request_job_control(&job.id, ControlCommand::Resume).unwrap();

        let (event_sender, mut event_receiver) = tokio::sync::mpsc::unbounded_channel();
        let signal = PauseSignal::new();
        signal.pause();
        let watcher = ControlFileWatcher::spawn_with_stop_cancel(
            db,
            job.id.clone(),
            Arc::new(RecordingSink {
                events: event_sender,
            }),
            signal,
            CancellationToken::new(),
            ControlBaseline {
                settings: TranslationProfile::V1Fast.resolve(),
                qa: QaMode::Off,
                validate_output: false,
            },
        );

        let events = tokio::time::timeout(TEST_DEADLOCK_TIMEOUT, async {
            let mut recorded = Vec::new();
            let mut saw_runtime_config = false;
            let mut saw_resume = false;
            loop {
                let event = event_receiver
                    .recv()
                    .await
                    .expect("watcher event channel should stay open");
                saw_runtime_config |= matches!(
                    &event,
                    ProgressEvent::RuntimeConfigChanged { revision: 9, .. }
                );
                saw_resume |= matches!(&event, ProgressEvent::JobResumed { .. });
                recorded.push(event);
                if saw_runtime_config && saw_resume {
                    break recorded;
                }
            }
        })
        .await
        .expect("watcher should publish and resume");

        let config_index = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    ProgressEvent::RuntimeConfigChanged { revision: 9, .. }
                )
            })
            .expect("runtime change should be recorded");
        let resume_index = events
            .iter()
            .position(|event| matches!(event, ProgressEvent::JobResumed { .. }))
            .expect("resume should be recorded");
        assert!(
            config_index < resume_index,
            "override revision must publish before Resume releases work"
        );
        assert_eq!(watcher.runtime_settings().borrow().revision, 9);

        drop(watcher);
        let _ = std::fs::remove_dir_all(run_dir);
    }

    #[test]
    fn runtime_launch_claim_deduplicates_concurrent_resume_attempts() {
        let job_id = format!("launch-claim-test-{}", now_ms());
        let run_dir = bookforge_core::run_dir_for_job(&job_id);
        let _ = std::fs::remove_dir_all(&run_dir);

        let acquire = || RuntimeLaunchClaim::acquire_with_stale_after(&job_id, Duration::MAX);
        let first = acquire()
            .expect("first claim should succeed")
            .expect("first caller should own the claim");
        assert!(
            acquire()
                .expect("second acquire should be readable")
                .is_none(),
            "a concurrent caller must not launch another worker"
        );

        drop(first);
        let mut persisted = acquire()
            .expect("claim should be reusable after an unlaunched owner drops")
            .expect("new caller should own the released claim");
        persisted.persist_until_worker();
        drop(persisted);
        assert!(
            acquire()
                .expect("persisted claim should remain readable")
                .is_none(),
            "a launched worker's claim must remain until its watcher clears it"
        );

        let _ = std::fs::remove_dir_all(run_dir);
    }

    #[test]
    fn runtime_launch_claim_reclaims_stale_claims_via_rename() {
        // CLI-7 regression: stale reclaim previously used check-then-delete,
        // where a concurrent acquirer could create a fresh claim between our
        // staleness check and the unlink and lose it. With rename-based
        // reclaim exactly one racer wins the rename itself.
        let job_id = format!("launch-claim-rename-test-{}", now_ms());
        let run_dir = bookforge_core::run_dir_for_job(&job_id);
        let _ = std::fs::remove_dir_all(&run_dir);
        let claim_path = run_dir.join("resume.launch");

        // Simulate a crashed worker's leftover claim file (no live owner, so
        // no in-process guard can clean it up).
        std::fs::create_dir_all(&run_dir).unwrap();
        std::fs::write(&claim_path, format!("{} {}", std::process::id(), now_ms())).unwrap();

        // A non-stale window never reaps an existing claim.
        assert!(
            RuntimeLaunchClaim::acquire_with_stale_after(&job_id, Duration::MAX)
                .expect("fresh scan should read cleanly")
                .is_none(),
            "claims inside the fresh window must survive"
        );
        assert!(claim_path.exists());

        // With an artificial always-stale deadline the leftover file is
        // reclaimed through a winning rename and replaced by our own claim.
        let reclaimed = RuntimeLaunchClaim::acquire_with_stale_after(&job_id, Duration::ZERO)
            .expect("reclaiming acquire should succeed")
            .expect("stale claim should be renamed out of the way");
        assert!(
            claim_path.exists(),
            "the winner recreates its own claim at the conventional path"
        );
        assert!(
            RuntimeLaunchClaim::acquire_with_stale_after(&job_id, Duration::MAX)
                .expect("post-reclaim read ok")
                .is_none(),
            "the recreated claim deduplicates like any fresh one"
        );
        drop(reclaimed);
        assert!(!claim_path.exists(), "drop removes the reclaimed claim");

        let _ = std::fs::remove_dir_all(run_dir);
    }

    #[test]
    fn runtime_lease_reader_reports_stale_and_invalid_files() {
        const OBSERVED_AT_MS: u64 = 10_000;
        let job_id = format!("runtime-lease-state-test-{}", now_ms());
        let run_dir = bookforge_core::run_dir_for_job(&job_id);
        let path = runtime_path_for_job(&job_id);
        let _ = std::fs::remove_dir_all(&run_dir);
        std::fs::create_dir_all(&run_dir).unwrap();

        let stale = RuntimeLease {
            schema_version: 1,
            instance_id: "stale-worker".to_string(),
            pid: 123,
            process_started_at_ms: 1,
            heartbeat_at_ms: OBSERVED_AT_MS
                .saturating_sub(RUNTIME_LEASE_STALE_AFTER.as_millis() as u64 + 1),
            last_loaded_revision: 2,
            last_applied_revision: 2,
        };
        std::fs::write(&path, serde_json::to_vec(&stale).unwrap()).unwrap();
        assert!(matches!(
            runtime_lease_state_at(&job_id, RUNTIME_LEASE_STALE_AFTER, OBSERVED_AT_MS),
            RuntimeLeaseState::Stale(lease) if lease.instance_id == "stale-worker"
        ));

        std::fs::write(&path, b"not-json").unwrap();
        assert!(matches!(
            runtime_lease_state_at(&job_id, RUNTIME_LEASE_STALE_AFTER, OBSERVED_AT_MS),
            RuntimeLeaseState::Invalid(_)
        ));

        let _ = std::fs::remove_dir_all(run_dir);
    }
}
