//! `bookforge retry`: mark failed / needs-review segments `retry_pending` and
//! then supervise a replacement worker that actually processes them.
//!
//! Before the audit this command only flipped segment state and told the
//! operator to run `resume` by hand. The real dogfooding run exposed what
//! happens when a replacement worker keeps dying silently instead: new PIDs
//! on every poll, zero events, zero log output, and no bounded end for 21+
//! minutes. The supervisor below is the fix for the parent side of that
//! failure — every child is announced ("replacement worker starting
//! (attempt N)"), every non-success exit is surfaced as a
//! `replacement_worker_died` error event, respawns back off exponentially
//! (1s, 2s, 4s, ... capped at 60s), and the loop is bounded: after
//! [`MAX_CONSECUTIVE_FAILURES`] consecutive dead children the command gives
//! up honestly and records an honest final state instead of looping forever.

use std::{
    io::Read,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::Result;
use bookforge_core::{ProgressEvent, ProgressSink, progress::now_ms};
use bookforge_store::{JobStore, JobSummary, RetryScope as StoreRetryScope};
use clap::{Args, ValueEnum};
use tokio_util::sync::CancellationToken;

use crate::progress::{ProgressReporter, UiMode};

/// Give up after this many consecutive dead replacement workers: the
/// supervisor must never loop unbounded.
const MAX_CONSECUTIVE_FAILURES: u32 = 5;
/// Exponential respawn backoff base (1s, 2s, 4s, ... capped).
const RESPAWN_BACKOFF_BASE: Duration = Duration::from_secs(1);
/// Backoff ceiling so a hopeless loop still stays calm.
const RESPAWN_BACKOFF_CAP: Duration = Duration::from_secs(60);
/// How much of a dead child's stderr is kept for the death report.
const STDERR_TAIL_BYTES: usize = 4096;

const TOKI_PONA_RETRY_VOCABULARY: &str = "a, akesi, ala, alasa, ale, ali, anpa, ante, anu, awen, e, en, epiku, esun, ijo, ike, ilo, insa, jaki, jan, jasima, jelo, jo, kala, kalama, kama, kasi, ken, kepeken, kijetesantakalu, kili, kin, kipisi, kiwen, ko, kokosila, kon, ku, kule, kulupu, kute, la, lanpan, lape, laso, lawa, leko, len, lete, li, lili, linja, linluwi, lipu, loje, lon, luka, lukin, lupa, ma, mama, mani, meli, meso, mi, mije, misikeke, moku, moli, monsi, monsuta, mu, mun, musi, mute, n, namako, nanpa, nasa, nasin, nena, ni, nimi, noka, o, oko, olin, ona, open, pake, pakala, pali, palisa, pan, pana, pi, pilin, pimeja, pini, pipi, poka, poki, pona, pu, sama, seli, selo, seme, sewi, sijelo, sike, sin, sina, sinpin, sitelen, soko, sona, soweli, suli, suno, supa, suwi, tan, taso, tawa, telo, tenpo, toki, tomo, tonsi, tu, unpa, uta, utala, walo, wan, waso, wawa, weka, wile";

#[derive(Debug, Args)]
pub struct RetryArgs {
    pub job_id: String,

    #[arg(long, value_enum, default_value_t = RetryScope::Failed)]
    pub only: RetryScope,

    /// How the supervised replacement worker reports progress. Omitted, the
    /// command behaves exactly as before `--ui` existed (auto: human output
    /// when stderr is a terminal, quiet otherwise).
    #[arg(long, value_enum)]
    pub ui: Option<UiMode>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum RetryScope {
    Failed,
    NeedsReview,
    All,
}

pub async fn run(args: RetryArgs, cancel_token: CancellationToken) -> Result<()> {
    let store = JobStore::open_default()?;
    let Some(job) = store.get_job(&args.job_id)? else {
        anyhow::bail!("job '{}' was not found", args.job_id);
    };

    let mut guided = 0usize;
    if job.target_lang.eq_ignore_ascii_case("Toki Pona")
        && matches!(args.only, RetryScope::NeedsReview | RetryScope::All)
    {
        for record in store.load_terminal_segment_translations(&args.job_id)? {
            if record.status != "needs_review" {
                continue;
            }
            let Some(error) = record.error.as_deref() else {
                continue;
            };
            let guidance = toki_pona_retry_guidance(error);
            store.request_segment_retry(&args.job_id, &record.segment_id, Some(&guidance))?;
            guided += 1;
        }
    }

    let count = guided + store.retry_segments(&args.job_id, args.only.into())?;
    // Machine-facing UI modes own stdout; the human chatter is suppressed for
    // them exactly like `resume` does.
    let human_stdout = args.ui.unwrap_or(UiMode::Auto).human_stdout();
    if human_stdout {
        println!("Job: {}", args.job_id);
        println!("Retry scope: {:?}", args.only);
        println!("Marked retry_pending: {count}");
        if guided > 0 {
            println!("Toki Pona error-guided retries: {guided}");
        }
    }
    if count == 0 {
        return Ok(());
    }

    // Preflight before any supervision: without a run snapshot the
    // replacement worker can only die repeatedly; a live worker or a
    // mid-launch rival means spawning would double-run the job. When the
    // preflight is clear it HANDS OVER the acquired launch claim so the
    // supervisor can pass it to each replacement worker via the environment.
    let launch_claim = match retry_launch_blocker(&store, &args.job_id)? {
        RetryPreflight::Blocked(reason) => anyhow::bail!("{reason}"),
        RetryPreflight::Ready(claim) => Some(claim),
    };

    let reporter = ProgressReporter::spawn_with_options(
        args.ui.unwrap_or(UiMode::Auto),
        None,
        true,
        Some(cancel_token.clone()),
    );
    let progress = reporter.sink();
    let supervision = supervise_replacement_worker(
        RetrySupervisor {
            job_id: args.job_id.clone(),
            progress,
            cancel: Some(cancel_token),
            launch_claim,
            ..RetrySupervisor::production()
        },
        &store,
    )
    .await;
    finalize_reporter(supervision, reporter).await
}

/// Combine the supervision result with the reporter's own shutdown result the
/// same way `resume` does, so a broken reporter cannot be swallowed.
async fn finalize_reporter(result: Result<()>, reporter: ProgressReporter) -> Result<()> {
    let reporter_result = reporter.shutdown().await;
    match (result, reporter_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(e), Ok(())) | (Ok(()), Err(e)) => Err(e),
        (Err(main_err), Err(progress_err)) => Err(anyhow::anyhow!(
            "{main_err}; additionally progress reporter failed: {progress_err}"
        )),
    }
}

fn toki_pona_retry_guidance(error: &str) -> String {
    let lower = error.to_ascii_lowercase();
    let text_only = lower.contains("marker")
        || lower.contains("run count mismatch")
        || lower.contains("unapproved lowercase word")
        || lower.contains("italian function words");
    let prefix = if text_only {
        "[bookforge:text-only] Inline formatting failed previously; return plain translated text and let BookForge restore the inline template. "
    } else {
        ""
    };
    let concise_error = error.chars().take(900).collect::<String>();
    format!(
        "{prefix}Return a fresh, complete Toki Pona translation. Correct every listed validation failure. Translate quoted prose and citation titles too; preserve only protected numbers, URLs, labels, acronyms, and proper names exactly once. Do not copy Italian or English prose and do not repeat text to fill the response. Except for protected source labels and capitalized proper names, use only this closed lowercase vocabulary: {TOKI_PONA_RETRY_VOCABULARY}. Previous validation: {concise_error}"
    )
}

/// [`respawn_backoff_within`] with the production base/cap: 1s, 2s, 4s, ...
/// capped at 60s. Used by the backoff test to pin the production spacing
/// without sleeping whole seconds.
#[cfg(test)]
fn respawn_backoff(consecutive_failures: u32) -> Duration {
    respawn_backoff_within(
        consecutive_failures,
        RESPAWN_BACKOFF_BASE,
        RESPAWN_BACKOFF_CAP,
    )
}

/// Exponential respawn backoff with injectable base/cap so tests can verify
/// the spacing without sleeping whole seconds: 1s, 2s, 4s, ... capped.
fn respawn_backoff_within(consecutive_failures: u32, base: Duration, cap: Duration) -> Duration {
    let multiplier = 2u32
        .saturating_pow(consecutive_failures.saturating_sub(1))
        .max(1);
    base.saturating_mul(multiplier).min(cap)
}

impl From<RetryScope> for StoreRetryScope {
    fn from(value: RetryScope) -> Self {
        match value {
            RetryScope::Failed => StoreRetryScope::Failed,
            RetryScope::NeedsReview => StoreRetryScope::NeedsReview,
            RetryScope::All => StoreRetryScope::All,
        }
    }
}

/// How a replacement worker process ended.
struct WorkerOutcome {
    /// Exit code 0 or 3 both mean the worker ran to completion (0: clean,
    /// 3: finished with failed/needs-review segments — an honest terminal
    /// state, not a dead worker). Everything else counts as a death.
    success: bool,
    /// The raw process exit code, so a code-3 completion can be propagated
    /// distinctly by the supervisor instead of collapsing into success.
    exit_code: Option<i32>,
    /// Human-readable status for the death report ("exit status: 1", ...).
    description: String,
    /// Bounded tail of the worker's stderr, for post-mortem context.
    stderr_tail: String,
}

pub(crate) struct RetrySupervisor {
    job_id: String,
    progress: Arc<dyn ProgressSink>,
    cancel: Option<CancellationToken>,
    /// The cross-process launch claim held by this launcher. Each replacement
    /// worker adopts it via the environment; between respawns the supervisor
    /// clears and re-acquires it so no child can ever race another launcher
    /// into a double-run. `None` when the preflight did not hand one over
    /// (test-only supervisors).
    launch_claim: Option<crate::control::RuntimeLaunchClaim>,
    max_consecutive_failures: u32,
    respawn_backoff_base: Duration,
    respawn_backoff_cap: Duration,
    /// Test hook (like the dashboard's `resume_launches`): when set, every
    /// spawn "dies" with this exit code instead of launching a real child, so
    /// tests can assert surfaced error events, backoff spacing, bounded
    /// termination, and the honest final state without processes.
    #[cfg(test)]
    forced_child_exit: Option<i32>,
}

impl RetrySupervisor {
    /// Production supervisor: five consecutive failures end the retry, with
    /// the 1s/2s/4s/.../60s respawn backoff.
    fn production() -> Self {
        Self {
            job_id: String::new(),
            progress: Arc::new(bookforge_core::NullProgressSink),
            cancel: None,
            launch_claim: None,
            max_consecutive_failures: MAX_CONSECUTIVE_FAILURES,
            respawn_backoff_base: RESPAWN_BACKOFF_BASE,
            respawn_backoff_cap: RESPAWN_BACKOFF_CAP,
            #[cfg(test)]
            forced_child_exit: None,
        }
    }
}

/// Supervise replacement workers until the job is processed or the bounded
/// failure budget is exhausted. Preflight checks (snapshot, live worker,
/// launch claim) run in [`retry_launch_blocker`] before this is called.
pub(crate) async fn supervise_replacement_worker(
    supervisor: RetrySupervisor,
    store: &JobStore,
) -> Result<()> {
    let RetrySupervisor {
        job_id,
        progress,
        cancel,
        launch_claim,
        max_consecutive_failures,
        respawn_backoff_base,
        respawn_backoff_cap,
        #[cfg(test)]
        forced_child_exit,
    } = supervisor;
    let mut launch_claim = launch_claim;
    // Snapshot the segment counts before the first spawn so the give-up path
    // can tell "the retry never started" from "progress was made and then a
    // later worker died" — the two demand different honest final states.
    let baseline = store.summary(&job_id)?;

    progress.emit(ProgressEvent::StageStarted {
        stage: "retry".to_string(),
        timestamp_ms: now_ms(),
    });

    let mut consecutive_failures = 0u32;
    let mut attempt = 0u32;
    let final_result = loop {
        attempt += 1;
        tracing::info!(
            job_id = %job_id,
            attempt,
            "replacement worker starting (attempt {attempt})"
        );

        // Re-arm the launch claim for every attempt after the first: the
        // previous handoff left it stale (its heartbeat stopped and the child
        // that adopted it is gone). Clearing the old nonce and acquiring fresh
        // closes the respawn gap the same way the initial preflight did.
        if attempt > 1
            && let Some(claim) = launch_claim.as_mut()
        {
            claim.clear();
            match crate::control::RuntimeLaunchClaim::acquire(&job_id)? {
                Some(fresh) => *claim = fresh,
                None => {
                    break Err(anyhow::anyhow!(
                        "another bookforge process took over the launch claim for job \
                         '{job_id}' while its replacement worker was down; refusing to \
                         double-run the job"
                    ));
                }
            }
        }

        // Verified parent-to-child handoff: stop our heartbeat, declare the
        // claim identity (job, nonce, owner pid) in the child's environment,
        // and let the child adopt — not re-acquire — the very same claim.
        if let Some(claim) = launch_claim.as_mut() {
            claim.handoff_to_child();
        }
        let outcome = {
            #[cfg(test)]
            if let Some(code) = forced_child_exit {
                spawn_forced_outcome(code)
            } else {
                spawn_and_wait_replacement_worker(&job_id, launch_claim.as_ref()).await
            }
            #[cfg(not(test))]
            {
                spawn_and_wait_replacement_worker(&job_id, launch_claim.as_ref()).await
            }
        };
        if outcome.success {
            if outcome.exit_code == Some(crate::exit_code::COMPLETED_WITH_FAILURES) {
                // The worker ran to completion but left unresolved segments:
                // propagate its distinct exit code (UI-21) so a script calling
                // `bookforge retry` can tell "finished cleanly" from "finished
                // with failures", exactly like a plain `resume`.
                crate::exit_code::request(crate::exit_code::COMPLETED_WITH_FAILURES);
            }
            break Ok(());
        }
        if cancel.as_ref().is_some_and(|cancel| cancel.is_cancelled()) {
            if let Some(claim) = launch_claim.as_mut() {
                claim.clear();
            }
            progress.emit(ProgressEvent::Warning {
                kind: "retry_supervision_cancelled".to_string(),
                message: "retry supervision cancelled; replacement worker left as-is".to_string(),
                timestamp_ms: now_ms(),
            });
            break Ok(());
        }

        consecutive_failures += 1;
        let message = format!(
            "replacement worker exited ({}): {}",
            outcome.description, outcome.stderr_tail
        );
        progress.emit(ProgressEvent::Error {
            kind: "replacement_worker_died".to_string(),
            message: message.clone(),
            timestamp_ms: now_ms(),
        });
        tracing::warn!(job_id = %job_id, attempt, consecutive_failures, "{message}");

        if consecutive_failures >= max_consecutive_failures {
            break Err(anyhow::anyhow!("{message}"));
        }

        let backoff = respawn_backoff_within(
            consecutive_failures,
            respawn_backoff_base,
            respawn_backoff_cap,
        );
        tracing::info!(
            job_id = %job_id,
            consecutive_failures,
            backoff_ms = backoff.as_millis() as u64,
            "backing off before the next replacement worker"
        );
        tokio::select! {
            () = tokio::time::sleep(backoff) => {}
            () = async {
                match cancel.as_ref() {
                    Some(cancel) => cancel.cancelled().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Some(claim) = launch_claim.as_mut() {
                    claim.clear();
                }
                progress.emit(ProgressEvent::Warning {
                    kind: "retry_supervision_cancelled".to_string(),
                    message: "retry supervision cancelled during respawn backoff".to_string(),
                    timestamp_ms: now_ms(),
                });
                break Ok(());
            }
        }
    };

    // No launcher path may leave a stale claim that would block an immediate
    // follow-up resume/retry from this still-live process (the pid guard would
    // otherwise hold it hostage until the hard-stale window passes).
    if let Some(claim) = launch_claim.as_mut() {
        claim.clear();
    }

    match final_result {
        Err(last_death) => honest_give_up(
            store,
            &job_id,
            baseline.as_ref(),
            consecutive_failures,
            &last_death,
        ),
        Ok(()) => {
            progress.emit(ProgressEvent::StageFinished {
                stage: "retry".to_string(),
                timestamp_ms: now_ms(),
            });
            Ok(())
        }
    }
}

/// Bounded give-up handling. When nothing progressed across the supervised
/// attempts the retry never actually started, so the job is marked failed
/// rather than left in `retry_pending` limbo; any progress is preserved and
/// the job is left exactly as the last worker left it. Both paths exit
/// non-zero with a clear message — the supervisor never fails silently.
fn honest_give_up(
    store: &JobStore,
    job_id: &str,
    baseline: Option<&JobSummary>,
    consecutive_failures: u32,
    last_death: &anyhow::Error,
) -> Result<()> {
    let summary = store.summary(job_id)?;
    let progressed = match (baseline, summary.as_ref()) {
        (Some(before), Some(after)) => {
            before.succeeded != after.succeeded
                || before.cached != after.cached
                || before.needs_review != after.needs_review
                || before.failed != after.failed
        }
        _ => false,
    };
    if !progressed {
        store.mark_job_failed(job_id)?;
        anyhow::bail!(
            "replacement worker failed {consecutive_failures} consecutive time(s) \
             ({last_death}); job '{job_id}' showed no progress and was marked failed"
        );
    }
    let status = summary.map(|summary| summary.status).unwrap_or_default();
    anyhow::bail!(
        "replacement worker failed {consecutive_failures} consecutive time(s) \
         ({last_death}); job '{job_id}' is left in status '{status}' with its progress preserved"
    );
}

/// Result of the preflight checks that must pass before the first replacement
/// worker is spawned.
enum RetryPreflight {
    /// Supervision cannot (or must not) start; carry the human reason.
    Blocked(String),
    /// Launch is clear: the caller now OWNS the cross-process launch claim and
    /// passes it to the supervisor, which hands it to each child via the
    /// environment.
    Ready(crate::control::RuntimeLaunchClaim),
}

/// Checks that must pass before the first replacement worker is spawned.
/// On success the acquired launch claim is returned (not dropped): a parent
/// that released it here would open a window for a concurrent resume to grab
/// it before the supervisor's first spawn — exactly the parent-to-child gap
/// the verified environment handoff exists to close.
fn retry_launch_blocker(store: &JobStore, job_id: &str) -> Result<RetryPreflight> {
    if store.load_job_config_snapshot(job_id)?.is_none() {
        return Ok(RetryPreflight::Blocked(format!(
            "job '{job_id}' has no run configuration snapshot and cannot be resumed by a \
             replacement worker"
        )));
    }
    if matches!(
        crate::control::runtime_lease_state(job_id, crate::control::RUNTIME_LEASE_STALE_AFTER),
        crate::control::RuntimeLeaseState::Fresh(_)
    ) {
        return Ok(RetryPreflight::Blocked(format!(
            "a live worker is already processing job '{job_id}'; not launching a replacement \
             (signal or stop that worker instead)"
        )));
    }
    // Cross-process dedupe (CLI-8): a held launch claim means another
    // resume/retry parent is mid-launch. Spawning on top of that hands the
    // two children a claim fight — the silent respawn loop observed while
    // dogfooding. Acquiring here (rather than merely peeking) also reclaims
    // any stale leftover claim and hands the fresh one to the supervisor.
    match crate::control::RuntimeLaunchClaim::acquire(job_id)? {
        Some(claim) => Ok(RetryPreflight::Ready(claim)),
        None => Ok(RetryPreflight::Blocked(format!(
            "another bookforge process appears to be launching or resuming job '{job_id}'; \
             refusing to double-run the job"
        ))),
    }
}

/// The launch protocol for one replacement worker.
///
/// Claim handoff (the audit's root-cause suspect): instead of releasing the
/// `resume.launch` claim before spawning and letting the child race for a
/// fresh one, the parent DECLARES its claim identity (job, nonce, owner pid)
/// in the child's environment. The child's resume entrypoint adopts that exact
/// claim — verified against the on-disk file — and holds it until its control
/// watcher establishes the runtime lease. A child therefore never fights the
/// parent (or a concurrent launcher) for the claim, and no other process can
/// slip into the parent-drop/child-acquire window.
async fn spawn_and_wait_replacement_worker(
    job_id: &str,
    claim: Option<&crate::control::RuntimeLaunchClaim>,
) -> WorkerOutcome {
    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(error) => {
            return WorkerOutcome {
                success: false,
                exit_code: None,
                description: "spawn failed".to_string(),
                stderr_tail: format!("failed to locate the BookForge executable: {error}"),
            };
        }
    };
    let mut command = std::process::Command::new(executable);
    command
        .arg("resume")
        .arg(job_id)
        .arg("--ui")
        .arg("quiet")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(claim) = claim {
        for (name, value) in claim.handoff_to_child_env() {
            command.env(name, value);
        }
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return WorkerOutcome {
                success: false,
                exit_code: None,
                description: "spawn failed".to_string(),
                stderr_tail: format!("failed to launch replacement worker: {error}"),
            };
        }
    };

    // Drain the child's stderr on a dedicated thread so a chatty worker can
    // never block on a full pipe; only a bounded tail is kept.
    let stderr_tail = Arc::new(Mutex::new(String::new()));
    if let Some(stderr) = child.stderr.take() {
        let tail = stderr_tail.clone();
        let spawned = std::thread::Builder::new()
            .name("bookforge-retry-child-stderr".to_string())
            .spawn(move || drain_stderr_tail(stderr, tail));
        if spawned.is_err() {
            let _ = child.kill();
            let _ = child.wait();
            return WorkerOutcome {
                success: false,
                exit_code: None,
                description: "spawn failed".to_string(),
                stderr_tail: "failed to start the stderr drain thread".to_string(),
            };
        }
    }

    let waited = {
        let tail = stderr_tail.clone();
        tokio::task::spawn_blocking(move || {
            let status = child.wait();
            (status, tail)
        })
        .await
    };

    match waited {
        Ok((Ok(status), tail)) => {
            let success = status.success()
                || status.code() == Some(crate::exit_code::COMPLETED_WITH_FAILURES);
            let stderr = tail.lock().map(|tail| tail.clone()).unwrap_or_default();
            WorkerOutcome {
                success,
                exit_code: status.code(),
                description: format!("{status}"),
                stderr_tail: stderr,
            }
        }
        Ok((Err(error), _)) => WorkerOutcome {
            success: false,
            exit_code: None,
            description: "wait failed".to_string(),
            stderr_tail: format!("failed to wait for the replacement worker: {error}"),
        },
        Err(error) => WorkerOutcome {
            success: false,
            exit_code: None,
            description: "wait failed".to_string(),
            stderr_tail: format!("replacement worker wait task failed: {error}"),
        },
    }
}

fn drain_stderr_tail(mut stderr: impl Read, tail: Arc<Mutex<String>>) {
    let mut buffer = [0u8; 1024];
    loop {
        match stderr.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(read) => {
                let Ok(mut tail) = tail.lock() else {
                    return;
                };
                tail.push_str(&String::from_utf8_lossy(&buffer[..read]));
                if tail.len() > STDERR_TAIL_BYTES {
                    // Keep the tail; cut at the first char boundary past the
                    // excess so multi-byte stderr output is never split.
                    let excess = tail.len() - STDERR_TAIL_BYTES;
                    let cut = (excess..=tail.len())
                        .find(|&index| tail.is_char_boundary(index))
                        .unwrap_or(tail.len());
                    tail.drain(..cut);
                }
            }
        }
    }
}

#[cfg(test)]
fn spawn_forced_outcome(code: i32) -> WorkerOutcome {
    WorkerOutcome {
        success: code == 0 || code == crate::exit_code::COMPLETED_WITH_FAILURES,
        exit_code: Some(code),
        description: format!("forced test exit status: {code}"),
        stderr_tail: format!("simulated child death (forced test exit {code})"),
    }
}

#[cfg(test)]
mod tests {
    use super::toki_pona_retry_guidance;

    #[test]
    fn toki_retry_guidance_uses_text_only_for_shape_and_foreign_prose_failures() {
        let marker = toki_pona_retry_guidance("inline marker missing: m1");
        assert!(marker.contains("[bookforge:text-only]"));
        assert!(marker.contains("inline marker missing: m1"));

        let grammar = toki_pona_retry_guidance("pi must group at least two following words");
        assert!(!grammar.contains("[bookforge:text-only]"));
        assert!(grammar.contains("pi must group at least two following words"));

        let foreign = toki_pona_retry_guidance(
            "unapproved lowercase word in strict Toki Pona output: stages",
        );
        assert!(foreign.contains("[bookforge:text-only]"));
        assert!(foreign.contains("Translate quoted prose and citation titles too"));
    }
}

#[cfg(test)]
mod supervisor_tests {
    use super::*;

    use bookforge_store::CreateJob;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[derive(Default)]
    struct RecordingSink {
        events: Mutex<Vec<ProgressEvent>>,
    }

    impl RecordingSink {
        fn events(&self) -> Vec<ProgressEvent> {
            self.events.lock().expect("events mutex").clone()
        }
    }

    impl ProgressSink for RecordingSink {
        fn emit(&self, event: ProgressEvent) {
            self.events.lock().expect("events mutex").push(event);
        }
    }

    fn temp_store(label: &str) -> (JobStore, std::path::PathBuf, String) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let db_path = std::env::temp_dir().join(format!(
            "bookforge-retry-{label}-{}-{nanos}-{}.sqlite",
            std::process::id(),
            TEST_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let store = JobStore::open(&db_path).expect("store opens");
        // create_job hashes the input file, so the fixture must exist.
        let input_path = std::env::temp_dir().join(format!(
            "bookforge-retry-input-{}-{nanos}-{}",
            std::process::id(),
            TEST_COUNTER.load(Ordering::Relaxed)
        ));
        std::fs::write(&input_path, b"epub bytes").expect("input fixture writes");
        let job = store
            .create_job(CreateJob {
                input: &input_path,
                output: std::path::Path::new("output.epub"),
                source_lang: Some("English"),
                target_lang: "Italian",
                provider: "mock",
                model: "mock-model",
                base_url: None,
                api_key_env: None,
                book_id: None,
                series_id: None,
            })
            .expect("job created");
        (store, db_path, job.id)
    }

    fn supervisor(
        job_id: &str,
        progress: Arc<dyn ProgressSink>,
        forced_child_exit: Option<i32>,
    ) -> RetrySupervisor {
        RetrySupervisor {
            job_id: job_id.to_string(),
            progress,
            cancel: None,
            launch_claim: None,
            max_consecutive_failures: MAX_CONSECUTIVE_FAILURES,
            respawn_backoff_base: Duration::from_millis(5),
            respawn_backoff_cap: Duration::from_millis(1000),
            forced_child_exit,
        }
    }

    fn death_events(events: &[ProgressEvent]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|event| match event {
                ProgressEvent::Error { kind, message, .. } if kind == "replacement_worker_died" => {
                    Some(message.as_str())
                }
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn supervisor_surfaces_deaths_backs_off_and_terminates_bounded() {
        let (store, db_path, job_id) = temp_store("bounded");
        let sink = Arc::new(RecordingSink::default());

        let started = std::time::Instant::now();
        let result =
            supervise_replacement_worker(supervisor(&job_id, sink.clone(), Some(1)), &store).await;
        let elapsed = started.elapsed();

        let error = result.expect_err("give-up must exit non-zero");
        let message = error.to_string();
        // Bounded: exactly MAX_CONSECUTIVE_FAILURES attempts, each surfaced.
        let events = sink.events();
        let deaths = death_events(&events);
        assert_eq!(deaths.len(), MAX_CONSECUTIVE_FAILURES as usize);
        for death in &deaths {
            assert!(death.contains("replacement worker exited"), "{death}");
            assert!(
                death.contains("simulated child death"),
                "stderr tail must be carried: {death}"
            );
        }
        // Honest final state: nothing progressed, so the job is marked
        // failed instead of being left in retry_pending limbo.
        assert!(message.contains("5 consecutive time(s)"), "{message}");
        assert!(message.contains("marked failed"), "{message}");
        assert_eq!(
            store.get_job(&job_id).expect("job").expect("exists").status,
            "failed"
        );
        // Backoff respected: 5 attempts need 4 backoffs (5+10+20+40 ms at the
        // injected base); a tight loop would finish far sooner.
        assert!(
            elapsed >= Duration::from_millis(75),
            "supervision finished in {elapsed:?}; backoff was not respected"
        );

        let _ = std::fs::remove_file(db_path);
        let _ = std::fs::remove_dir_all(bookforge_core::run_dir_for_job(&job_id));
    }

    #[tokio::test]
    async fn supervisor_stops_quietly_when_a_worker_completes() {
        for code in [0, crate::exit_code::COMPLETED_WITH_FAILURES] {
            let (store, db_path, job_id) = temp_store("success");
            let sink = Arc::new(RecordingSink::default());

            let result =
                supervise_replacement_worker(supervisor(&job_id, sink.clone(), Some(code)), &store)
                    .await;

            assert!(result.is_ok(), "a completed worker must not fail: {code}");
            assert!(
                death_events(&sink.events()).is_empty(),
                "a completed worker must not surface death events"
            );
            // The job row is left exactly as the worker left it.
            assert_eq!(
                store.get_job(&job_id).expect("job").expect("exists").status,
                "running"
            );
            let _ = std::fs::remove_file(db_path);
            let _ = std::fs::remove_dir_all(bookforge_core::run_dir_for_job(&job_id));
        }
    }

    #[test]
    fn launch_blocker_refuses_jobs_without_a_run_snapshot() {
        let (store, db_path, job_id) = temp_store("nosnapshot");

        let blocker = retry_launch_blocker(&store, &job_id).expect("blocker reads cleanly");

        match blocker {
            RetryPreflight::Blocked(reason) => {
                assert!(
                    reason.contains("run configuration snapshot"),
                    "blocker reason should name the missing snapshot: {reason}"
                );
            }
            RetryPreflight::Ready(_) => {
                panic!("a job without a snapshot must be blocked");
            }
        }
        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn give_up_marks_failed_only_when_nothing_progressed() {
        let (store, db_path, job_id) = temp_store("giveup");

        // Nothing progressed (baseline == current): the retry never started,
        // so the job is marked failed.
        let baseline = store.summary(&job_id).expect("summary");
        let result = honest_give_up(
            &store,
            &job_id,
            baseline.as_ref(),
            5,
            &anyhow::anyhow!("replacement worker exited (exit status: 1): boom"),
        );
        let message = result.expect_err("give-up is always an error").to_string();
        assert!(message.contains("marked failed"), "{message}");
        assert_eq!(
            store.get_job(&job_id).expect("job").expect("exists").status,
            "failed"
        );
        let _ = std::fs::remove_file(db_path);

        // Progress happened before the deaths: the job keeps its state and
        // the message preserves the reason.
        let (store, db_path, job_id) = temp_store("giveup_progress");
        let mut baseline = store.summary(&job_id).expect("summary").expect("exists");
        baseline.succeeded += 3;
        let result = honest_give_up(
            &store,
            &job_id,
            Some(&baseline),
            5,
            &anyhow::anyhow!("replacement worker exited (exit status: 1): boom"),
        );
        let message = result.expect_err("give-up is always an error").to_string();
        assert!(message.contains("progress preserved"), "{message}");
        assert_eq!(
            store.get_job(&job_id).expect("job").expect("exists").status,
            "running",
            "progressed jobs are never clobbered by the give-up path"
        );
        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn respawn_backoff_doubles_and_caps() {
        assert_eq!(respawn_backoff(1), Duration::from_secs(1));
        assert_eq!(respawn_backoff(2), Duration::from_secs(2));
        assert_eq!(respawn_backoff(3), Duration::from_secs(4));
        assert_eq!(respawn_backoff(4), Duration::from_secs(8));
        assert_eq!(respawn_backoff(7), RESPAWN_BACKOFF_CAP);
        assert_eq!(respawn_backoff(50), RESPAWN_BACKOFF_CAP);
    }

    /// UI-21: a child that exits 3 (completed but with unresolved segments) is
    /// an honest terminal state — no respawn, no death event — but the retry
    /// command must propagate the distinct code so scripts can tell it apart
    /// from a clean 0.
    #[tokio::test]
    async fn supervisor_propagates_child_exit_code_3_distinctly() {
        crate::exit_code::request(crate::exit_code::SUCCESS);
        let (store, db_path, job_id) = temp_store("exit3");
        let sink = Arc::new(RecordingSink::default());

        let result =
            supervise_replacement_worker(supervisor(&job_id, sink.clone(), Some(3)), &store).await;

        assert!(
            result.is_ok(),
            "a code-3 completion must not fail the retry"
        );
        assert!(
            death_events(&sink.events()).is_empty(),
            "a code-3 completion is not a death"
        );
        assert_eq!(
            crate::exit_code::requested_code(),
            crate::exit_code::COMPLETED_WITH_FAILURES,
            "the child's distinct exit code 3 must propagate"
        );
        crate::exit_code::request(crate::exit_code::SUCCESS);
        let _ = std::fs::remove_file(db_path);
        let _ = std::fs::remove_dir_all(bookforge_core::run_dir_for_job(&job_id));
    }

    /// The preflight hands the acquired launch claim to the supervisor so the
    /// parent-to-child handoff can use it; a clean preflight for a retryable
    /// job must produce a live claim, and the claim's handoff env must carry
    /// the job id plus a non-empty nonce and owner pid.
    #[tokio::test]
    async fn launch_blocker_hands_over_a_claim_with_handoff_identity() {
        let (store, db_path, job_id) = temp_store("handoff");
        store
            .update_job_config_snapshot(
                &job_id,
                &bookforge_core::RunConfigSnapshot {
                    input_path: std::path::PathBuf::from("input.epub"),
                    input_snapshot_path: None,
                    input_sha256: None,
                    output_path: std::path::PathBuf::from("out.epub"),
                    events_path: None,
                    report_json_path: None,
                    report_markdown_path: None,
                    source_language: Some("English".to_string()),
                    target_language: "Italian".to_string(),
                    creator: None,
                    provider: "mock".to_string(),
                    model: "mock-model".to_string(),
                    base_url: None,
                    api_key_env: None,
                    profile: bookforge_core::TranslationProfile::V1Fast,
                    provider_preset: None,
                    prompt_version: "v1".to_string(),
                    cache_namespace: "ns".to_string(),
                    book_id: None,
                    series_id: None,
                    glossary_budget_tokens: 800,
                    glossary_format: bookforge_core::GlossaryFormat::Json,
                    prompt_extra: None,
                    glossary_fingerprint: String::new(),
                    glossary_terms: Vec::new(),
                    context_window: 0,
                    context_budget_tokens: 1200,
                    context_scope: bookforge_core::config::ContextScope::Chapter,
                    style_fingerprint: String::new(),
                    style_rendered_block: String::new(),
                    entities_fingerprint: String::new(),
                    entities_rendered_block: String::new(),
                    bilingual_mode: bookforge_core::BilingualMode::Replace,
                    bilingual_separator: " / ".to_string(),
                    bilingual_style: bookforge_core::BilingualStyle::Minimal,
                    bilingual_css: None,
                    fallback: None,
                    finalize: bookforge_core::FinalizeCheckpointSnapshot::default(),
                    qa_mode: "off".to_string(),
                    validate_output: false,
                    settings: bookforge_core::ResolvedRunSettingsSnapshot::from_settings(
                        &bookforge_core::TranslationProfile::V1Fast.resolve(),
                    ),
                },
            )
            .expect("snapshot should write");

        let claim = match retry_launch_blocker(&store, &job_id).expect("preflight reads cleanly") {
            RetryPreflight::Ready(claim) => claim,
            RetryPreflight::Blocked(reason) => {
                panic!("retryable job must not be blocked: {reason}")
            }
        };
        let env = claim.handoff_to_child_env();
        let lookup = env
            .iter()
            .map(|(name, value)| (*name, value.clone()))
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(
            lookup
                .get(crate::control::LAUNCH_CLAIM_ENV_JOB)
                .expect("job var"),
            &job_id
        );
        assert!(
            !lookup
                .get(crate::control::LAUNCH_CLAIM_ENV_NONCE)
                .expect("nonce var")
                .is_empty(),
            "nonce must not be empty"
        );
        assert!(
            lookup
                .get(crate::control::LAUNCH_CLAIM_ENV_PID)
                .expect("pid var")
                .parse::<u32>()
                .expect("pid is numeric")
                > 0,
            "owner pid must be present"
        );

        drop(claim);
        let _ = std::fs::remove_file(db_path);
        let _ = std::fs::remove_dir_all(bookforge_core::run_dir_for_job(&job_id));
    }

    /// The supervisor's launch-claim lifecycle (lifecycle audit): the claim the
    /// preflight hands over is cleared and re-acquired between respawns (so no
    /// child can race a rival into a double-run through a stale parent claim),
    /// and a give-up leaves NO stale claim behind that would block an immediate
    /// follow-up resume/retry from this still-live process.
    #[tokio::test]
    async fn supervisor_clears_its_launch_claim_through_the_respawn_loop() {
        let (store, db_path, job_id) = temp_store("claim-loop");
        let claim_path = bookforge_core::run_dir_for_job(&job_id).join("resume.launch");

        let claim = crate::control::RuntimeLaunchClaim::acquire(&job_id)
            .expect("acquire reads cleanly")
            .expect("the preflight owns the claim");
        let sink = Arc::new(RecordingSink::default());
        let mut supervisor = supervisor(&job_id, sink.clone(), Some(1));
        supervisor.launch_claim = Some(claim);

        let result = supervise_replacement_worker(supervisor, &store).await;
        assert!(result.is_err(), "give-up must exit non-zero");

        assert!(
            !claim_path.exists(),
            "a given-up supervisor must clear its launch claim so a follow-up \
             resume/retry is never blocked by its own stale claim"
        );
        // The claim path must also not be littered with parked/remove debris.
        let run_dir = bookforge_core::run_dir_for_job(&job_id);
        let debris = std::fs::read_dir(&run_dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains("resume.launch")
            })
            .count();
        assert_eq!(debris, 0, "no claim debris may accumulate across respawns");

        let _ = std::fs::remove_file(db_path);
        let _ = std::fs::remove_dir_all(run_dir);
    }
}
