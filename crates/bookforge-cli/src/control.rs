use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::Result;
use bookforge_core::{
    ControlCommand, ProgressEvent, ProgressSink, ResolvedRunSettings, clear_control_file,
    control_path_for_job, now_ms,
};
use bookforge_llm::{EngineRuntimeSettings, PauseSignal, PauseState, TranslationRunConfig};
use bookforge_store::JobStore;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::QaMode;

const CONTROL_POLL_INTERVAL: Duration = Duration::from_millis(100);
pub(crate) const RUNTIME_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
pub(crate) const RUNTIME_LEASE_STALE_AFTER: Duration = Duration::from_secs(3);
/// Heartbeat age beyond which a launch claim is a reclaim candidate. The owner
/// refreshes this heartbeat on its own schedule, so a claim can no longer
/// expire purely from file mtime while the owner is legitimately starting up
/// (CLI audit: a slow startup used to lose its claim to a 10 s mtime window).
const RUNTIME_LAUNCH_CLAIM_STALE_AFTER: Duration = Duration::from_secs(10);
const LAUNCH_CLAIM_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(500);
const LAUNCH_CLAIM_SCHEMA_VERSION: u32 = 1;
static RUNTIME_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Cross-process launch-claim handoff environment variables. A retry
/// supervisor acquires the claim and passes its identity to the replacement
/// worker so the child adopts the same claim instead of racing to create a
/// fresh one (closes the parent-to-child handoff gap).
pub(crate) const LAUNCH_CLAIM_ENV_JOB: &str = "BOOKFORGE_LAUNCH_CLAIM_JOB";
pub(crate) const LAUNCH_CLAIM_ENV_NONCE: &str = "BOOKFORGE_LAUNCH_CLAIM_NONCE";
pub(crate) const LAUNCH_CLAIM_ENV_PID: &str = "BOOKFORGE_LAUNCH_CLAIM_PID";

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
    if age_ms <= duration_ms(stale_after) {
        return RuntimeLeaseState::Fresh(lease);
    }
    // The heartbeat is stale, but the lease is only reclaimable when its owner
    // is positively dead. A live-but-suspended/slow owner still holds the lease
    // (fail-closed): reclaiming at the heartbeat window would let a retry
    // spawn a replacement worker over a live one. An owner whose liveness
    // cannot be established is treated as ALIVE forever — takeover is never
    // authorized by age alone, only by a positively established death.
    match pid_liveness(lease.pid) {
        OwnerLiveness::Alive => RuntimeLeaseState::Fresh(lease),
        OwnerLiveness::Gone => RuntimeLeaseState::Stale(lease),
        OwnerLiveness::Indeterminate => RuntimeLeaseState::Fresh(lease),
    }
}

/// Whether [`atomic_replace`] may overwrite an existing destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplaceMode {
    /// Install over whatever is already at the destination.
    Replace,
    /// Install only when the destination does not exist yet (a rival already
    /// writing the same sidecar wins and the caller retries the read path).
    CreateExclusive,
}

/// The one, shared, crash-conscious writer for every lifecycle sidecar:
/// control commands, the runtime lease, launch claims, and the overrides
/// document (`reconfigure.rs`).
///
/// Content is staged into a uniquely named temp file inside the destination
/// directory, fsynced, and only then installed, so a crash or failed write can
/// never leave a truncated sidecar at the canonical path:
/// - [`ReplaceMode::Replace`] renames over the destination (atomic on Unix;
///   Windows uses `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` which atomically
///   replaces without ever removing the previous file first, so a crash or
///   power loss never destroys the only good state before publication).
/// - [`ReplaceMode::CreateExclusive`] installs through a hard link so exactly
///   one caller wins and no rival's freshly written claim is ever overwritten;
///   filesystems without hard-link support fall back to an exclusive
///   `create_new` open of the same bytes.
///
/// After a successful install the parent directory is fsynced (Unix) so the
/// directory entry is durable too. On any failure the destination is left
/// exactly as it was and the staged temp file is removed (failure-preserving).
pub(crate) fn atomic_replace(path: &Path, bytes: &[u8], mode: ReplaceMode) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let suffix = RUNTIME_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("sidecar");
    let staged = path.with_file_name(format!(".{file_name}.staged-{}-{suffix}", process::id()));
    let install = || -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&staged)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        match mode {
            ReplaceMode::Replace => rename_replacing(&staged, path),
            ReplaceMode::CreateExclusive => link_exclusive(&staged, path, bytes),
        }?;
        sync_parent_dir(path);
        Ok(())
    };
    let result = install();
    // Always drop the staged temp: `Replace` renamed it away (no-op), but
    // `CreateExclusive` leaves it in place after a successful hard link and
    // both modes leave it behind on failure.
    let _ = fs::remove_file(&staged);
    result
}

/// Best-effort fsync of the directory holding `path`, so the directory entry
/// for a freshly installed sidecar is durable on crash, not just the file
/// contents. Only Linux supports opening+syncing a directory; elsewhere this
/// is deliberately a silent no-op (Windows cannot open a directory as a file,
/// and macOS/BSD reject the fsync with EINVAL).
fn sync_parent_dir(path: &Path) {
    #[cfg(unix)]
    {
        let Some(parent) = path.parent() else {
            return;
        };
        if let Ok(dir) = fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// Atomic rename over `path`. Unix `rename(2)` replaces atomically.
#[cfg(not(windows))]
fn rename_replacing(staged: &Path, path: &Path) -> std::io::Result<()> {
    fs::rename(staged, path)
}

/// Atomic replace over an existing `path` on Windows, where `rename` refuses
/// to overwrite. `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` replaces the
/// destination in one atomic operation with no window in which the destination
/// is missing, and on failure leaves the previous file byte-for-byte intact.
/// The caller cleans the staged temp; the destination is never touched when
/// the API fails, so a crash/power loss can never destroy the only good state
/// before publication.
#[cfg(windows)]
fn rename_replacing(staged: &Path, path: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    let from: Vec<u16> = staged.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: both arguments are NUL-terminated wide strings that outlive the
    // call, and the path arguments are independently validated by the OS.
    let replaced = unsafe {
        windows_sys::Win32::Storage::FileSystem::MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING,
        )
    };
    if replaced != 0 {
        return Ok(());
    }
    // MoveFileExW failed: surface the OS error. No rename-aside fallback is
    // attempted — it would briefly remove the destination (widening the crash
    // window and risking the only good state on power loss), which is exactly
    // what the fail-closed writer exists to prevent.
    Err(std::io::Error::last_os_error())
}

/// Exclusive install of `staged` at `path`. A hard link is atomic and fails
/// with `AlreadyExists` when a rival won the race; platforms/filesystems
/// without hard-link support fall back to an exclusive `create_new` open.
fn link_exclusive(staged: &Path, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    match fs::hard_link(staged, path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Err(error),
        Err(_) => {
            let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
            file.write_all(bytes)?;
            file.sync_all()
        }
    }
}

fn write_runtime_lease(path: &Path, lease: &RuntimeLease) -> Result<()> {
    let json = serde_json::to_string_pretty(lease)?;
    atomic_replace(path, format!("{json}\n").as_bytes(), ReplaceMode::Replace)?;
    Ok(())
}

/// Poll interval while waiting for a held [`ProcessFileLock`].
const PROCESS_LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(25);

/// A cross-process advisory lock whose ownership lives in the KERNEL
/// (`flock(2)` on Unix, `LockFileEx` on Windows), not in a removable lock
/// file. Used for the overrides document in `reconfigure.rs`.
///
/// This deliberately replaces a `create_new` + mtime protocol: the lock file
/// is opened (created once if missing) and never unlinked or recreated during
/// ordinary acquire/release, so its inode identity is stable for every
/// contender. Exclusivity is enforced by the OS on the open descriptor:
/// - the kernel releases the lock when the holding process dies (its
///   descriptors close), so a crashed owner's lock is recovered automatically
///   — no pid/age heuristic, and **age never authorizes takeover**;
/// - a contender waits up to `wait` and then fails clearly;
/// - releasing (dropping) closes only OUR descriptor and never unlinks the
///   file, so it cannot affect a successor's lock.
///
/// The owner record written into the file is informational only (for `cat`
/// diagnostics) and is never read to decide ownership, so unreadable/foreign
/// records fail closed (the kernel lock is the sole authority).
#[derive(Debug)]
pub(crate) struct ProcessFileLock {
    /// Held open for the guard's lifetime: the kernel lock lives on this
    /// descriptor and is released when it closes (on guard drop / process
    /// death). Intentionally never read — its sole purpose is the open fd.
    _file: File,
}

impl ProcessFileLock {
    /// Acquire the exclusive kernel lock on `path`, retrying until `wait`
    /// elapses. Creates the file (and its parent) when missing. Fails clearly
    /// when the lock is still held after the window.
    pub(crate) fn acquire(path: &Path, wait: Duration) -> Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let deadline = Instant::now() + wait;
        let file = OpenOptions::new()
            .create(true)
            // The lock file persists and must NEVER be truncated: its inode
            // and any informational owner record stay valid across releases.
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| {
                anyhow::anyhow!("failed to open lock file {}: {error}", path.display())
            })?;
        loop {
            match try_lock_exclusive(&file) {
                Ok(()) => {
                    // Informational owner record; never consulted for ownership.
                    let _ = write_owner_record(&file);
                    return Ok(Self { _file: file });
                }
                Err(error) if is_lock_contention(&error) => {
                    if Instant::now() >= deadline {
                        anyhow::bail!(
                            "timed out waiting {} ms for runtime override lock {}; \
                             another process holds it",
                            wait.as_millis(),
                            path.display()
                        );
                    }
                    thread::sleep(PROCESS_LOCK_RETRY_INTERVAL);
                }
                Err(error) => {
                    return Err(anyhow::anyhow!(
                        "failed to lock {}: {error}",
                        path.display()
                    ));
                }
            }
        }
    }
}

/// Best-effort owner record for diagnostics. `overrides.json` writes are
/// protected by the kernel lock, never by this text.
fn write_owner_record(file: &File) -> std::io::Result<()> {
    use std::io::{Seek, Write as _};
    // Use the already locked handle. On Windows the byte-range lock can block
    // overlapping access through a separately duplicated handle.
    let mut file = file;
    let record = format!("pid={}\nacquired_at_ms={}\n", process::id(), now_ms());
    file.set_len(0)?;
    file.seek(std::io::SeekFrom::Start(0))?;
    file.write_all(record.as_bytes())?;
    file.flush()?;
    file.sync_all()
}

/// Try once to take the exclusive lock, failing immediately if it is held.
#[cfg(unix)]
fn try_lock_exclusive(file: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Try once to take the exclusive lock, failing immediately if it is held.
#[cfg(windows)]
fn try_lock_exclusive(file: &File) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{
        LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;

    let handle = file.as_raw_handle() as HANDLE;
    let mut overlapped = OVERLAPPED::default();
    let result = unsafe {
        LockFileEx(
            handle,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if result != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(any(unix, windows)))]
fn try_lock_exclusive(_file: &File) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "kernel-backed file locking is unsupported on this platform",
    ))
}

/// Whether a failed lock attempt means "someone else holds it" (retryable).
#[cfg(unix)]
fn is_lock_contention(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::WouldBlock
}

#[cfg(windows)]
fn is_lock_contention(error: &std::io::Error) -> bool {
    use windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION;
    error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32)
}

#[cfg(not(any(unix, windows)))]
fn is_lock_contention(_error: &std::io::Error) -> bool {
    false
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

fn launch_claim_path(job_id: &str) -> PathBuf {
    bookforge_core::run_dir_for_job(job_id).join("resume.launch")
}

/// Durable identity carried by a cross-process launch claim. Older builds wrote
/// a plain `"{pid} {ms}"` line; [`read_launch_claim_document`] tolerates that
/// legacy format so a crashed worker's leftover claim is still reclaimed.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct LaunchClaimDocument {
    schema_version: u32,
    nonce: String,
    pid: u32,
    created_at_ms: u64,
    heartbeat_at_ms: u64,
}

fn read_launch_claim_document(path: &Path) -> Option<LaunchClaimDocument> {
    let contents = fs::read_to_string(path).ok()?;
    if let Ok(doc) = serde_json::from_str::<LaunchClaimDocument>(&contents) {
        return (doc.schema_version == LAUNCH_CLAIM_SCHEMA_VERSION).then_some(doc);
    }
    let mut parts = contents.split_whitespace();
    let pid = parts.next()?.parse::<u32>().ok()?;
    let ms = parts.next()?.parse::<u64>().ok()?;
    Some(LaunchClaimDocument {
        schema_version: LAUNCH_CLAIM_SCHEMA_VERSION,
        nonce: format!("legacy-{pid}"),
        pid,
        created_at_ms: ms,
        heartbeat_at_ms: ms,
    })
}

fn write_launch_claim_document(path: &Path, doc: &LaunchClaimDocument) -> Result<()> {
    let bytes = serde_json::to_vec(doc)?;
    atomic_replace(path, &bytes, ReplaceMode::Replace)?;
    Ok(())
}

/// Compare-and-replace for the launch claim (heartbeat + adoption): proves
/// that the current file still carries `expected_nonce` and publishes the new
/// document through the single atomic writer. Returns `true` when the
/// replacement was published, `false` when the nonce no longer matches (the
/// claim was reclaimed or cleared since the caller's earlier read).
///
/// This is what closes the heartbeat/reclaim TOCTOU: a stale owner can never
/// overwrite a newer claim, because the ownership proof happens immediately
/// before — and the publication is — one atomic rename. The only window left
/// (between the verify-read and the rename) is closed by the reclaim policy:
/// a provably live owner is never reclaimable, a positively dead owner is not
/// heartbeating a live thread, and an owner whose liveness is Indeterminate is
/// never reclaimed at all — so no rival publication can appear between the
/// verify-read and the rename.
fn replace_launch_claim_if_owned(
    path: &Path,
    expected_nonce: &str,
    new_doc: &LaunchClaimDocument,
) -> Result<bool> {
    let Some(current) = read_launch_claim_document(path) else {
        return Ok(false);
    };
    if current.nonce != expected_nonce {
        return Ok(false);
    }
    write_launch_claim_document(path, new_doc)?;
    Ok(true)
}

/// Liveness of a pid encoded in a launch claim/lease. Mirrors the startup
/// sweep's probe (`main.rs::owner_liveness`) on Unix: Linux `/proc` is
/// authoritative, other Unixes shell out to `ps`. On Windows the probe is a
/// real OS handle query ([`pid_liveness_windows`]) so death is positively
/// established rather than guessed. Only a platform with NO liveness probe at
/// all reports [`OwnerLiveness::Indeterminate`], which callers fail closed on
/// forever (takeover is never authorized by age).
///
/// `pub(crate)` so the startup sweep in `main.rs` reuses the same robust,
/// cross-platform semantics instead of duplicating an age-based heuristic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OwnerLiveness {
    Alive,
    Gone,
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    Indeterminate,
}

pub(crate) fn pid_liveness(pid: u32) -> OwnerLiveness {
    if pid == process::id() {
        return OwnerLiveness::Alive;
    }

    #[cfg(target_os = "linux")]
    {
        if PathBuf::from(format!("/proc/{pid}")).exists() {
            OwnerLiveness::Alive
        } else {
            OwnerLiveness::Gone
        }
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    {
        match process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "comm="])
            .output()
        {
            Ok(output) if output.status.success() && !output.stdout.trim().is_empty() => {
                OwnerLiveness::Alive
            }
            Ok(_) => OwnerLiveness::Gone,
            Err(_) => OwnerLiveness::Indeterminate,
        }
    }

    #[cfg(windows)]
    {
        pid_liveness_windows(pid)
    }

    #[cfg(not(any(unix, windows)))]
    {
        OwnerLiveness::Indeterminate
    }
}

/// Robust Windows liveness probe: `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)`
/// either opens the process (it exists) or fails. A pid that does not exist
/// fails with `ERROR_INVALID_PARAMETER` — positive proof of death. A process
/// that exists is then distinguished via `GetExitCodeProcess`: `STILL_ACTIVE`
/// means alive, a concrete exit code means it has terminated. Any other probe
/// failure (access denied) cannot be resolved and is reported as
/// [`OwnerLiveness::Indeterminate`], which callers fail closed on forever.
#[cfg(windows)]
fn pid_liveness_windows(pid: u32) -> OwnerLiveness {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        // ERROR_INVALID_PARAMETER is what the OS returns for a pid with no
        // process object at all; any other failure means we cannot tell.
        return match std::io::Error::last_os_error().raw_os_error() {
            Some(code) if code == ERROR_INVALID_PARAMETER as i32 => OwnerLiveness::Gone,
            _ => OwnerLiveness::Indeterminate,
        };
    }
    let mut exit_code: u32 = 0;
    let probed = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
    // Best-effort close: the handle is only used for the two calls above.
    unsafe { CloseHandle(handle) };
    if probed == 0 {
        return OwnerLiveness::Indeterminate;
    }
    if exit_code == STILL_ACTIVE as u32 {
        OwnerLiveness::Alive
    } else {
        OwnerLiveness::Gone
    }
}

/// Whether an existing claim may be reclaimed. Reclaim requires BOTH a stale
/// heartbeat AND an owner that is POSITIVELY dead: a provably live pid is NEVER
/// reclaimed merely for heartbeat age, and an owner whose liveness cannot be
/// established ([`OwnerLiveness::Indeterminate`]) is treated as live forever.
/// Age alone never authorizes takeover — only a positively established death
/// does, and only then at the short launch window. This is what keeps a
/// slow-starting/suspended but healthy worker from losing its claim to a
/// concurrent launcher; the nonce checks close the pid-reuse/TOCTOU hole on
/// the write side.
fn launch_claim_is_reclaimable(
    doc: &LaunchClaimDocument,
    stale_after: Duration,
    observed_at_ms: u64,
) -> bool {
    launch_claim_is_reclaimable_for_liveness(
        pid_liveness(doc.pid),
        observed_at_ms.saturating_sub(doc.heartbeat_at_ms),
        stale_after,
    )
}

/// Pure reclaim decision (kept separate so the fail-closed policy is
/// mutation-provable on every platform without probing real pids):
/// - [`OwnerLiveness::Alive`]: NEVER reclaimed for heartbeat age alone.
/// - [`OwnerLiveness::Gone`]: reclaim once the heartbeat is past `stale_after`.
/// - [`OwnerLiveness::Indeterminate`]: NEVER reclaimed. The owner's death is
///   not positively established (e.g. the probe itself failed), so the claim
///   stays protected indefinitely — reclaiming it could duplicate a live
///   suspended/future-schema owner, and no age threshold makes that safe.
fn launch_claim_is_reclaimable_for_liveness(
    liveness: OwnerLiveness,
    age_ms: u64,
    stale_after: Duration,
) -> bool {
    match liveness {
        OwnerLiveness::Alive => false,
        OwnerLiveness::Gone => age_ms >= duration_ms(stale_after),
        OwnerLiveness::Indeterminate => false,
    }
}

/// Duration in whole milliseconds, saturating on overflow.
fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Atomically move `path` out of the way and unlink it. Exactly one racer wins
/// the rename, so a concurrent acquirer can never lose a freshly created claim
/// between a staleness check and an unlink; losers either observe `NotFound`
/// (the winner moved it first) or a fresh claim and back off.
fn reclaim_claim_file(path: &Path) -> Result<bool> {
    let reclaimed = path.with_file_name(format!(
        ".resume.launch.reclaimed-{}-{}",
        process::id(),
        RUNTIME_FILE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    match fs::rename(path, &reclaimed) {
        Ok(()) => {
            let _ = fs::remove_file(&reclaimed);
            Ok(true)
        }
        Err(_) => Ok(false),
    }
}

/// Background heartbeat for a launch claim. Writes the owner's heartbeat on a
/// fixed cadence so the claim stays fresh across the whole startup window, and
/// stops itself as soon as the claim file disappears or its nonce changes (the
/// worker's watcher removed it or another process reclaimed it).
struct LaunchClaimHeartbeat {
    cancel: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl LaunchClaimHeartbeat {
    fn start(path: PathBuf, nonce: String) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let thread_cancel = cancel.clone();
        let thread_path = path.clone();
        let thread_nonce = nonce.clone();
        let join = thread::Builder::new()
            .name("bookforge-launch-claim-heartbeat".to_string())
            .spawn(move || {
                loop {
                    if thread_cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    thread::sleep(LAUNCH_CLAIM_HEARTBEAT_INTERVAL);
                    if thread_cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    let Some(doc) = read_launch_claim_document(&thread_path) else {
                        // The worker's watcher removed the claim (or another
                        // process reclaimed it): stop heartbeating it.
                        break;
                    };
                    if doc.nonce != thread_nonce {
                        break;
                    }
                    let refreshed = LaunchClaimDocument {
                        schema_version: LAUNCH_CLAIM_SCHEMA_VERSION,
                        nonce: thread_nonce.clone(),
                        pid: process::id(),
                        created_at_ms: doc.created_at_ms,
                        heartbeat_at_ms: now_ms(),
                    };
                    // Publish through the compare-and-replace helper: the
                    // ownership proof is repeated immediately before the
                    // atomic rename, so a stale heartbeat can never overwrite
                    // a newer claim. A transient write error is retried next
                    // tick; a nonce mismatch means the claim is no longer ours.
                    match replace_launch_claim_if_owned(&thread_path, &thread_nonce, &refreshed) {
                        Ok(true) => {}
                        Ok(false) => break,
                        Err(_) => {}
                    }
                }
            })
            .expect("launch claim heartbeat thread should spawn");
        Self {
            cancel,
            join: Some(join),
        }
    }

    fn stop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for LaunchClaimHeartbeat {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A cross-process claim that prevents two bookforge processes from launching
/// a worker for the same job at once.
///
/// Unlike the previous mtime-only design, the claim carries owner identity
/// (nonce + pid + creation timestamp) and a heartbeat the owner refreshes, and
/// removal is always owner-checked: `Drop` and [`RuntimeLaunchClaim::clear`]
/// only unlink the file while the stored nonce still matches ours, so a
/// non-owner can never delete someone else's claim.
pub(crate) struct RuntimeLaunchClaim {
    path: PathBuf,
    job_id: String,
    nonce: String,
    remove_on_drop: bool,
    heartbeat: Option<LaunchClaimHeartbeat>,
}

impl RuntimeLaunchClaim {
    pub(crate) fn acquire(job_id: &str) -> Result<Option<Self>> {
        Self::acquire_with_stale_after(job_id, RUNTIME_LAUNCH_CLAIM_STALE_AFTER)
    }

    fn acquire_with_stale_after(job_id: &str, stale_after: Duration) -> Result<Option<Self>> {
        let path = launch_claim_path(job_id);
        for _ in 0..2 {
            let observed_at = now_ms();
            let nonce = generate_claim_nonce()?;
            let doc = LaunchClaimDocument {
                schema_version: LAUNCH_CLAIM_SCHEMA_VERSION,
                nonce: nonce.clone(),
                pid: process::id(),
                created_at_ms: observed_at,
                heartbeat_at_ms: observed_at,
            };
            // Exclusive install through the shared atomic writer: a rival's
            // freshly written claim is never overwritten (hard-link install
            // fails with AlreadyExists), and our own claim is never observed
            // half-written.
            match atomic_replace(
                &path,
                &serde_json::to_vec(&doc)?,
                ReplaceMode::CreateExclusive,
            ) {
                Ok(()) => {
                    return Ok(Some(Self {
                        path: path.clone(),
                        job_id: job_id.to_string(),
                        nonce: nonce.clone(),
                        remove_on_drop: true,
                        heartbeat: Some(LaunchClaimHeartbeat::start(path, nonce)),
                    }));
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    match read_launch_claim_document(&path) {
                        Some(doc) if !launch_claim_is_reclaimable(&doc, stale_after, now_ms()) => {
                            return Ok(None);
                        }
                        // A parseable claim whose owner is POSITIVELY dead:
                        // reclaim via a winning rename so exactly one racer
                        // wins and a fresh claim is never lost between the
                        // check and an unlink.
                        Some(_) => {
                            if reclaim_claim_file(&path)? {
                                continue;
                            }
                            return Ok(None);
                        }
                        // An unparseable claim (garbage or a foreign schema)
                        // carries no readable owner identity, so the owner's
                        // death can never be positively established. Fail
                        // closed INDEFINITELY: no age threshold may authorize
                        // stealing a live future-schema owner's claim.
                        None => return Ok(None),
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(None)
    }

    /// Adopt the launch claim a parent process handed off via environment
    /// variables (retry supervisor -> replacement worker). Returns `None` when
    /// no handoff is in flight for this job or the claim file is missing / no
    /// longer matches, in which case the caller falls back to a fresh acquire.
    ///
    /// The adoption is VERIFIED: the on-disk claim must carry the exact job,
    /// nonce, and owner pid the parent declared. A claim that was reclaimed by
    /// someone else, or whose pid stopped matching the env (stale env reused
    /// by a later process), is deliberately NOT adopted — a foreign claim must
    /// never be taken over on the strength of a stale environment.
    pub(crate) fn adopt_from_env(job_id: &str) -> Result<Option<Self>> {
        if std::env::var(LAUNCH_CLAIM_ENV_JOB).ok().as_deref() != Some(job_id) {
            return Ok(None);
        }
        let Some(expected_nonce) = std::env::var(LAUNCH_CLAIM_ENV_NONCE).ok() else {
            return Ok(None);
        };
        let expected_pid = std::env::var(LAUNCH_CLAIM_ENV_PID)
            .ok()
            .and_then(|value| value.parse::<u32>().ok());
        let path = launch_claim_path(job_id);
        let Some(doc) = read_launch_claim_document(&path) else {
            return Ok(None);
        };
        if doc.nonce != expected_nonce {
            return Ok(None);
        }
        if expected_pid.is_some_and(|pid| doc.pid != pid) {
            return Ok(None);
        }
        let nonce = doc.nonce.clone();
        let created_at_ms = doc.created_at_ms;
        // Take over ownership: the claim's liveness now points at THIS process.
        // The rewrite goes through the compare-and-replace helper so a claim
        // that was reclaimed between our read above and this write is never
        // clobbered — the adoption then fails closed and the caller falls back
        // to a fresh acquire (or is blocked by whoever holds the new claim).
        let adopted = LaunchClaimDocument {
            schema_version: LAUNCH_CLAIM_SCHEMA_VERSION,
            nonce: nonce.clone(),
            pid: process::id(),
            created_at_ms,
            heartbeat_at_ms: now_ms(),
        };
        if !replace_launch_claim_if_owned(&path, &nonce, &adopted)? {
            return Ok(None);
        }
        Ok(Some(Self {
            path: path.clone(),
            job_id: job_id.to_string(),
            nonce: nonce.clone(),
            remove_on_drop: true,
            heartbeat: Some(LaunchClaimHeartbeat::start(path, nonce)),
        }))
    }

    /// Environment variables that hand this claim to a child process for
    /// [`RuntimeLaunchClaim::adopt_from_env`]: the job id, the nonce, and the
    /// owner pid the child must verify against the on-disk claim.
    pub(crate) fn handoff_to_child_env(&self) -> [(&'static str, String); 3] {
        [
            (LAUNCH_CLAIM_ENV_JOB, self.job_id.clone()),
            (LAUNCH_CLAIM_ENV_NONCE, self.nonce.clone()),
            (LAUNCH_CLAIM_ENV_PID, process::id().to_string()),
        ]
    }

    /// Keep the claim durable beyond this object's drop (the worker's watcher
    /// is responsible for clearing it). The heartbeat keeps running. Used only
    /// by the dashboard's exactly-once test hook, which never spawns a child.
    #[cfg(test)]
    pub(crate) fn persist_until_worker(&mut self) {
        self.remove_on_drop = false;
    }

    /// Hand the claim to a child process: stop heartbeating it ourselves while
    /// leaving the file in place for the child to adopt and eventually clear.
    pub(crate) fn handoff_to_child(&mut self) {
        self.remove_on_drop = false;
        if let Some(heartbeat) = self.heartbeat.as_mut() {
            heartbeat.stop();
        }
    }

    /// Stop heartbeating and unlink the claim only while we still own it.
    pub(crate) fn clear(&mut self) {
        if let Some(heartbeat) = self.heartbeat.as_mut() {
            heartbeat.stop();
        }
        self.remove_if_owned();
    }

    /// Unlink the claim file only when the stored nonce still matches ours.
    ///
    /// The canonical path is first moved aside atomically and the ownership
    /// check runs on the parked file, so the unlink targets exactly the file
    /// we verified — never a newer claim a rival installed at the canonical
    /// path between our read and our removal (late-Drop safety for the
    /// parent-to-child handoff).
    pub(crate) fn remove_if_owned(&mut self) {
        let parked = self.path.with_file_name(format!(
            ".resume.launch.remove-{}-{}",
            process::id(),
            RUNTIME_FILE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        if fs::rename(&self.path, &parked).is_err() {
            return;
        }
        let owned = read_launch_claim_document(&parked).is_some_and(|doc| doc.nonce == self.nonce);
        if owned {
            let _ = fs::remove_file(&parked);
        } else if fs::rename(&parked, &self.path).is_err() {
            // A rival republished at the canonical path while ours was parked:
            // the parked copy is superseded, never the live claim.
            let _ = fs::remove_file(&parked);
        }
    }
}

/// Cryptographically random 128-bit claim identity, hex-encoded. The nonce is
/// what makes a claim tamper-resistant across processes: pid reuse can never
/// confuse a nonce check, so a claimant that merely shares a pid with a stale
/// claim cannot adopt or remove someone else's. A nonce is a hard prerequisite
/// — if the OS random source fails, claim acquisition FAILS rather than
/// falling back to a predictable counter identity (a guessable nonce would
/// defeat the very TOCTOU/pid-reuse protection the nonce exists to provide).
fn generate_claim_nonce() -> Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| {
        anyhow::anyhow!("failed to generate a cryptographic launch-claim nonce: {error}")
    })?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

impl Drop for RuntimeLaunchClaim {
    fn drop(&mut self) {
        if let Some(heartbeat) = self.heartbeat.as_mut() {
            heartbeat.stop();
        }
        if self.remove_on_drop {
            self.remove_if_owned();
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

/// Write a control command through the shared atomic-replace helper so a
/// reader can never observe a partially-written control file (a partial
/// `pau…` must never be mistaken for the default `Run`).
fn write_control_file_atomic(path: &Path, command: ControlCommand) -> Result<()> {
    let contents = format!("{}\n", command.as_str());
    atomic_replace(path, contents.as_bytes(), ReplaceMode::Replace)?;
    Ok(())
}

/// Strict control-file reader: a malformed or partial control file is an
/// explicit error — never silently treated as `Run` (which would resume a
/// paused worker). Missing files stay the default `Run`.
fn read_control_file_strict(path: &Path) -> Result<ControlCommand> {
    match fs::read_to_string(path) {
        Ok(contents) => match contents.trim() {
            "pause" => Ok(ControlCommand::Pause),
            "resume" | "run" => Ok(ControlCommand::Resume),
            "stop" => Ok(ControlCommand::Stop),
            other => anyhow::bail!("malformed control file at {}: {:?}", path.display(), other),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(ControlCommand::Run),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn request_job_control(job_id: &str, command: ControlCommand) -> Result<PathBuf> {
    let path = control_path_for_job(job_id);
    write_control_file_atomic(&path, command)?;
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
        // Strict read: a malformed/partial control file must surface as an
        // explicit error instead of being decoded as Run (fail-closed).
        match read_control_file_strict(&self.path)? {
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
    handle: Option<std::thread::JoinHandle<()>>,
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
    ) -> Result<Self> {
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
    ) -> Result<Self> {
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
        // Fail-closed: without a durable runtime lease no worker can be told
        // apart from a crashed one, so the run must not start at all.
        write_runtime_lease(&lease_path, &lease).map_err(|error| {
            anyhow::anyhow!("failed to create runtime lease for job '{job_id}': {error}")
        })?;
        if let Some(loaded) = initial_loaded.as_ref() {
            progress.emit(ProgressEvent::RuntimeConfigChanged {
                revision: loaded.revision,
                changed_fields: loaded.overrides.changed_fields(),
                application: loaded.overrides.application_boundaries(),
                timestamp_ms: now_ms(),
            });
        }
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
        Ok(Self {
            cancel,
            handle: Some(handle),
            runtime_settings,
            job_runtime_settings,
            lease_path,
            lease_instance_id,
            #[cfg(test)]
            heartbeat_updates,
        })
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
        // Signal the poll thread and JOIN it before removing the lease, so no
        // final heartbeat can land after the lease file is gone (the thread's
        // exit path removes the lease itself; our removal is the deterministic
        // backstop once the thread is finished).
        self.cancel.cancel();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        remove_runtime_lease_if_owned(&self.lease_path, &self.lease_instance_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookforge_core::{NullProgressSink, TranslationProfile, read_control_file};

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
    fn poller_treats_missing_control_as_running_and_garbage_as_an_error() {
        // Finding: malformed/partial control must be an explicit error/stop-safe
        // state, NEVER decoded as Run (which would resume a paused worker).
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

        // Missing control file is the default Run (pause is lifted).
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

        // A garbage/partial control file is an explicit error and must NOT
        // release the paused state (never treated as Run).
        std::fs::write(&control_path, "pau").unwrap();
        signal.pause();
        let error = poller
            .poll(&signal)
            .expect_err("malformed control file must surface as an error");
        assert!(
            error.to_string().contains("malformed control file"),
            "unexpected error: {error}"
        );
        assert_eq!(
            signal.state(),
            PauseState::Paused,
            "a malformed control file must never release a paused worker"
        );
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
        )
        .expect("watcher should spawn");
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
        )
        .expect("watcher should spawn");

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
        )
        .expect("watcher should spawn");

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
        // no in-process guard can clean it up). The owner pid is deliberately
        // dead — under the heartbeat/pid-aware policy a claim owned by a LIVE
        // pid is never reclaimed, so a dead pid is what proves staleness here.
        std::fs::create_dir_all(&run_dir).unwrap();
        let dead_pid = u32::MAX - 7;
        std::fs::write(&claim_path, format!("{dead_pid} {}", now_ms())).unwrap();

        // A non-stale window never reaps an existing claim (even a dead
        // owner's: the heartbeat has not aged past the requested window).
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

    /// Concurrent acquire across many threads must yield EXACTLY one winner
    /// while the claim is held: the exclusive hard-link install means no two
    /// callers can ever observe the same freshly created claim, and the losers
    /// all read the winner's fresh (non-reclaimable) claim and back off. The
    /// winner's claim is held across all joins (like production holds it for
    /// the whole launch window), so a late racer can never win a second time
    /// after an early winner dropped its claim.
    #[test]
    fn concurrent_claim_acquire_has_exactly_one_winner() {
        let job_id = format!("launch-claim-race-{}-{}", process::id(), now_ms());
        let run_dir = bookforge_core::run_dir_for_job(&job_id);
        let _ = std::fs::remove_dir_all(&run_dir);

        const RACERS: usize = 8;
        let barrier = Arc::new(std::sync::Barrier::new(RACERS));
        let mut handles = Vec::with_capacity(RACERS);
        for _ in 0..RACERS {
            let barrier = barrier.clone();
            let job_id = job_id.clone();
            handles.push(
                std::thread::Builder::new()
                    .name(format!("claim-racer-{job_id}"))
                    .spawn(move || {
                        barrier.wait();
                        RuntimeLaunchClaim::acquire_with_stale_after(&job_id, Duration::MAX)
                            .expect("acquire should never error on a clean race")
                    })
                    .expect("racer thread spawns"),
            );
        }
        let won = handles
            .into_iter()
            .map(|handle| handle.join().expect("racer joins"))
            .collect::<Vec<_>>();
        assert_eq!(
            won.iter().filter(|claim| claim.is_some()).count(),
            1,
            "exactly one concurrent acquirer must win the claim"
        );
        assert!(
            run_dir.join("resume.launch").exists(),
            "the winner's claim remains until the caller releases it"
        );
        // The winner's claim is held through all joins, so it is released only
        // here — dropping it must cleanly remove the claim file.
        drop(won);
        assert!(
            !run_dir.join("resume.launch").exists(),
            "releasing the winner's claim removes it"
        );

        let _ = std::fs::remove_dir_all(run_dir);
    }

    /// A claim owned by a PROVABLY LIVE pid must never be reclaimed merely
    /// because its heartbeat looks old: an owner that is alive (but, say, slow
    /// to heartbeat) stays protected until the hard-stale window, closing the
    /// pid-reuse/TOCTOU hole a pure heartbeat-age policy would open.
    #[test]
    fn live_pid_claim_is_never_reclaimed_for_heartbeat_age_alone() {
        let job_id = format!("launch-claim-livepid-{}-{}", process::id(), now_ms());
        let run_dir = bookforge_core::run_dir_for_job(&job_id);
        let _ = std::fs::remove_dir_all(&run_dir);
        let claim_path = run_dir.join("resume.launch");
        std::fs::create_dir_all(&run_dir).unwrap();

        // Our own pid is provably alive right now; the heartbeat is ancient
        // (hard-stale and beyond), but the owner is live, so even an
        // always-stale deadline must NOT reclaim it.
        let doc = LaunchClaimDocument {
            schema_version: LAUNCH_CLAIM_SCHEMA_VERSION,
            nonce: "legacy-live-owner".to_string(),
            pid: process::id(),
            created_at_ms: 1,
            heartbeat_at_ms: 1,
        };
        std::fs::write(&claim_path, serde_json::to_vec(&doc).unwrap()).unwrap();

        assert!(
            RuntimeLaunchClaim::acquire_with_stale_after(&job_id, Duration::ZERO)
                .expect("live-pid scan should read cleanly")
                .is_none(),
            "a provably live owner must never be reclaimed for heartbeat age alone"
        );
        assert!(
            claim_path.exists(),
            "the live owner's claim must survive untouched"
        );

        let _ = std::fs::remove_dir_all(run_dir);
    }

    /// The shared atomic writer is failure-preserving: when the write fails,
    /// the destination keeps its prior contents and no staged temp file leaks.
    #[test]
    fn atomic_replace_preserves_prior_contents_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sidecar.json");
        std::fs::write(&path, b"prior-good").unwrap();

        atomic_replace(&path, b"replacement", ReplaceMode::Replace)
            .expect("a clean replace should succeed");
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement");

        // A non-empty directory sitting where the file must land makes the
        // final rename fail; the directory itself must stay completely
        // untouched and the staged temp file must be cleaned up.
        let blocked = dir.path().join("blocked");
        std::fs::create_dir_all(&blocked).unwrap();
        std::fs::write(blocked.join("inner"), b"keep-me").unwrap();
        let failed = atomic_replace(&blocked, b"overwrite", ReplaceMode::Replace)
            .expect_err("replacing over a non-empty directory must fail");
        assert!(!failed.to_string().is_empty());
        assert_eq!(
            std::fs::read(blocked.join("inner")).unwrap(),
            b"keep-me",
            "a failed replace must preserve the prior contents"
        );
        let staged_left = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().contains("staged"));
        assert!(
            !staged_left,
            "a failed replace must clean up its staged temp file"
        );
    }

    /// The exclusive create mode installs a fresh file and refuses to clobber
    /// an existing one (the claim-dedupe guarantee), while the replace mode
    /// overwrites the same destination without any leftover staging.
    #[test]
    fn atomic_replace_exclusive_vs_replace_semantics() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("claim");

        atomic_replace(&path, b"first", ReplaceMode::CreateExclusive).expect("first create wins");
        let error = atomic_replace(&path, b"second", ReplaceMode::CreateExclusive)
            .expect_err("a second exclusive install must be refused");
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::AlreadyExists,
            "the rival's claim must never be overwritten"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"first",
            "the original claim content survives"
        );

        // Replace mode overwrites the same path cleanly.
        atomic_replace(&path, b"third", ReplaceMode::Replace).expect("replace overwrites");
        assert_eq!(std::fs::read(&path).unwrap(), b"third");
        let staged_left = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().contains("staged"));
        assert!(!staged_left, "no staged files may leak after any install");
    }

    /// Nonces must come from the OS CSPRNG as 32 hex chars — never a
    /// predictable counter/time identity (which would defeat the TOCTOU and
    /// pid-reuse protection the nonce exists for).
    #[test]
    fn claim_nonce_is_cryptographic_hex() {
        let a = generate_claim_nonce().expect("nonce generation must succeed");
        let b = generate_claim_nonce().expect("nonce generation must succeed");
        assert_eq!(a.len(), 32, "128-bit nonce is 32 hex chars: {a}");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()), "hex only: {a}");
        assert_ne!(a, b, "two draws must differ");
    }

    /// Fail-closed reclaim (lifecycle audit): an `Indeterminate` owner (whose
    /// death the platform cannot positively establish) is NEVER reclaimed at
    /// any heartbeat age — takeover is authorized only by a positively
    /// established death, never by age. A `Gone` owner is reclaimed once the
    /// heartbeat is past the short window; an `Alive` owner is never reclaimed
    /// for heartbeat age alone.
    #[test]
    fn indeterminate_liveness_is_never_reclaimed_at_any_age() {
        let short = RUNTIME_LAUNCH_CLAIM_STALE_AFTER;
        // An arbitrary huge age far beyond any plausible hard-age window.
        let huge_age = duration_ms(short) * 1_000_000 + 1;

        // Indeterminate is never reclaimed, regardless of heartbeat age.
        assert!(!launch_claim_is_reclaimable_for_liveness(
            OwnerLiveness::Indeterminate,
            huge_age,
            short,
        ));
        assert!(!launch_claim_is_reclaimable_for_liveness(
            OwnerLiveness::Indeterminate,
            duration_ms(short) + 1,
            short,
        ));

        // A provably dead owner reclaims at the short window...
        assert!(launch_claim_is_reclaimable_for_liveness(
            OwnerLiveness::Gone,
            duration_ms(short),
            short,
        ));
        // ...but not before it.
        assert!(!launch_claim_is_reclaimable_for_liveness(
            OwnerLiveness::Gone,
            duration_ms(short) - 1,
            short,
        ));

        // A provably live owner is never reclaimed, even at huge ages.
        assert!(!launch_claim_is_reclaimable_for_liveness(
            OwnerLiveness::Alive,
            huge_age,
            short,
        ));
    }

    /// Compare-and-replace (lifecycle audit): the heartbeat/adoption writer
    /// must prove ownership immediately before publication. When the claim
    /// file no longer carries our nonce, the replacement is refused and the
    /// newer claim is left byte-for-byte intact.
    #[test]
    fn replace_launch_claim_if_owned_never_publishes_over_a_reclaimed_claim() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resume.launch");
        let mine = LaunchClaimDocument {
            schema_version: LAUNCH_CLAIM_SCHEMA_VERSION,
            nonce: "our-nonce".to_string(),
            pid: 1,
            created_at_ms: 1,
            heartbeat_at_ms: 1,
        };
        write_launch_claim_document(&path, &mine).expect("claim writes");

        // A matching nonce publishes the refreshed document.
        let refreshed = LaunchClaimDocument {
            schema_version: LAUNCH_CLAIM_SCHEMA_VERSION,
            nonce: "our-nonce".to_string(),
            pid: 2,
            created_at_ms: 1,
            heartbeat_at_ms: 99,
        };
        assert!(
            replace_launch_claim_if_owned(&path, "our-nonce", &refreshed).expect("replace reads"),
            "an owned claim must be published"
        );
        let on_disk = read_launch_claim_document(&path).expect("claim readable");
        assert_eq!(on_disk.pid, 2, "the refresh was published");

        // Simulate the TOCTOU: another process reclaimed the claim (new nonce)
        // between our read and our write. The compare-and-replace MUST refuse
        // and leave the newer claim untouched.
        let rival = LaunchClaimDocument {
            schema_version: LAUNCH_CLAIM_SCHEMA_VERSION,
            nonce: "rival-nonce".to_string(),
            pid: 7,
            created_at_ms: 500,
            heartbeat_at_ms: 500,
        };
        write_launch_claim_document(&path, &rival).expect("rival claim writes");
        let stale_refresh = LaunchClaimDocument {
            schema_version: LAUNCH_CLAIM_SCHEMA_VERSION,
            nonce: "our-nonce".to_string(),
            pid: 3,
            created_at_ms: 1,
            heartbeat_at_ms: 999,
        };
        assert!(
            !replace_launch_claim_if_owned(&path, "our-nonce", &stale_refresh)
                .expect("stale replace reads cleanly"),
            "a stale owner must never overwrite a newer claim"
        );
        let after = read_launch_claim_document(&path).expect("rival claim survives");
        assert_eq!(
            after.nonce, "rival-nonce",
            "the rival's claim survives untouched"
        );
        assert_eq!(
            after.pid, 7,
            "no stale bytes may leak into the rival's claim"
        );
        assert_eq!(after.heartbeat_at_ms, 500, "heartbeat must not be touched");
    }

    /// Late-Drop safety (lifecycle audit): `remove_if_owned` must unlink only
    /// the claim whose nonce matches ours. A rival's claim installed at the
    /// canonical path is never deleted, and removal leaves no parked debris.
    #[test]
    fn remove_if_owned_never_deletes_a_rival_claim() {
        let job_id = format!("remove-owned-test-{}", now_ms());
        let run_dir = bookforge_core::run_dir_for_job(&job_id);
        let _ = std::fs::remove_dir_all(&run_dir);
        std::fs::create_dir_all(&run_dir).unwrap();

        let path = run_dir.join("resume.launch");
        let mut claim = RuntimeLaunchClaim {
            path: path.clone(),
            job_id: job_id.clone(),
            nonce: "our-nonce".to_string(),
            remove_on_drop: true,
            heartbeat: None,
        };
        // A rival's claim occupies the path.
        let rival = LaunchClaimDocument {
            schema_version: LAUNCH_CLAIM_SCHEMA_VERSION,
            nonce: "rival-nonce".to_string(),
            pid: 42,
            created_at_ms: 1,
            heartbeat_at_ms: 1,
        };
        write_launch_claim_document(&path, &rival).expect("rival claim writes");
        claim.remove_if_owned();
        let after = read_launch_claim_document(&path).expect("rival claim survives");
        assert_eq!(
            after.nonce, "rival-nonce",
            "a rival's claim must never be deleted"
        );

        // Our own claim is removed cleanly with no parked debris.
        let ours = LaunchClaimDocument {
            schema_version: LAUNCH_CLAIM_SCHEMA_VERSION,
            nonce: "our-nonce".to_string(),
            pid: process::id(),
            created_at_ms: 2,
            heartbeat_at_ms: 2,
        };
        write_launch_claim_document(&path, &ours).expect("our claim writes");
        claim.remove_if_owned();
        assert!(!path.exists(), "our own claim is removed");
        let leftovers = std::fs::read_dir(&run_dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains("remove-"))
            .count();
        assert_eq!(leftovers, 0, "no parked claim debris may accumulate");

        let _ = std::fs::remove_dir_all(run_dir);
    }

    /// Fail-closed reclaim (lifecycle audit): a claim file that cannot be
    /// parsed carries no owner identity, so the owner's death can never be
    /// positively established. It is NEVER reclaimed at any age — reclaiming
    /// it could steal a live future-schema owner's claim, and no mtime
    /// threshold makes that safe.
    #[test]
    fn unparseable_claim_blocks_reclaim_indefinitely() {
        let job_id = format!("unparseable-job-{}-{}", process::id(), now_ms());
        let run_dir = bookforge_core::run_dir_for_job(&job_id);
        let _ = std::fs::remove_dir_all(&run_dir);
        std::fs::create_dir_all(&run_dir).unwrap();
        let path = run_dir.join("resume.launch");
        std::fs::write(&path, b"not a claim at all").unwrap();

        // An unparseable claim blocks every acquisition, even with an
        // always-stale deadline (fail-closed: no double-run).
        assert!(
            RuntimeLaunchClaim::acquire_with_stale_after(&job_id, Duration::ZERO)
                .expect("acquire reads cleanly")
                .is_none(),
            "an unparseable claim must never be reclaimed"
        );
        assert!(
            path.exists(),
            "the unparseable claim must survive untouched"
        );

        let _ = std::fs::remove_dir_all(run_dir);
    }

    /// Fail-closed runtime lease (lifecycle audit): a lease whose heartbeat is
    /// stale but whose owner pid is PROVABLY ALIVE (a suspended/slow worker)
    /// must stay Fresh — reclaiming it at the heartbeat window would let a
    /// retry spawn a second worker over a live one.
    #[test]
    fn runtime_lease_with_live_pid_stays_fresh_when_heartbeat_is_stale() {
        const OBSERVED_AT_MS: u64 = 10_000;
        let job_id = format!("runtime-lease-livepid-test-{}", now_ms());
        let run_dir = bookforge_core::run_dir_for_job(&job_id);
        let path = runtime_path_for_job(&job_id);
        let _ = std::fs::remove_dir_all(&run_dir);
        std::fs::create_dir_all(&run_dir).unwrap();

        // Our own pid is provably alive; the heartbeat is far past the stale
        // window, simulating a suspended but live worker.
        let live = RuntimeLease {
            schema_version: 1,
            instance_id: "suspended-worker".to_string(),
            pid: process::id(),
            process_started_at_ms: 1,
            heartbeat_at_ms: 1,
            last_loaded_revision: 0,
            last_applied_revision: 0,
        };
        std::fs::write(&path, serde_json::to_vec(&live).unwrap()).unwrap();
        assert!(
            matches!(
                runtime_lease_state_at(&job_id, RUNTIME_LEASE_STALE_AFTER, OBSERVED_AT_MS),
                RuntimeLeaseState::Fresh(lease) if lease.instance_id == "suspended-worker"
            ),
            "a live (suspended) owner's lease must stay Fresh despite the stale heartbeat"
        );

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
            // A pid that is never alive on any real box, so the lease is
            // provably stale under the pid-aware policy (a live pid would
            // keep the lease Fresh even with an old heartbeat).
            pid: u32::MAX - 7,
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

    // ---------------------------------------------------------------------
    // ProcessFileLock: kernel-backed overrides lock (reconfigure.rs)
    // ---------------------------------------------------------------------

    /// Two concurrent acquirers contend on the kernel lock: the loser times
    /// out clearly while the winner holds it, and dropping the winner releases
    /// it so the next acquirer succeeds. The lock file is never unlinked.
    #[test]
    fn process_file_lock_contends_and_releases_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("overrides.lock");

        let held = ProcessFileLock::acquire(&lock_path, Duration::from_secs(5))
            .expect("first acquirer wins");
        let error = ProcessFileLock::acquire(&lock_path, Duration::from_millis(150))
            .expect_err("a held lock must not be acquired");
        assert!(
            error.to_string().contains("timed out"),
            "the timeout error must be explicit: {error}"
        );
        assert!(lock_path.exists(), "the lock file is never removed");

        drop(held);
        let reacquired = ProcessFileLock::acquire(&lock_path, Duration::from_secs(5))
            .expect("dropping the holder releases the kernel lock");
        assert!(lock_path.exists(), "release does not unlink the lock file");
        drop(reacquired);
    }

    /// A live holder's lock is never stolen, even when the lock file looks
    /// ancient: age plays no role in the protocol (fail closed on takeover).
    #[test]
    fn process_file_lock_is_never_stolen_by_file_age() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("overrides.lock");

        let held = ProcessFileLock::acquire(&lock_path, Duration::from_secs(5))
            .expect("first acquirer wins");
        // Age the lock file far beyond any plausible stale window.
        let old = std::time::SystemTime::now()
            .checked_sub(Duration::from_secs(100_000))
            .expect("clock sane");
        let times = std::fs::FileTimes::new().set_modified(old);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&lock_path)
            .expect("lock file opens")
            .set_times(times)
            .expect("mtime ages");
        let error = ProcessFileLock::acquire(&lock_path, Duration::from_millis(150))
            .expect_err("an aged lock file must not authorize takeover");
        assert!(error.to_string().contains("timed out"), "{error}");
        drop(held);
    }

    /// The kernel releases the lock when the holder's descriptor closes —
    /// exactly what happens on process death — so a crashed owner's lock is
    /// recovered without any pid/age heuristic.
    #[test]
    fn process_file_lock_is_released_by_the_kernel_on_handle_close() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("overrides.lock");

        let holder = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .expect("lock file opens");
        try_lock_exclusive(&holder).expect("holder takes the kernel lock");
        let contender = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .expect("contender opens the same inode");
        let blocked = try_lock_exclusive(&contender)
            .expect_err("a second descriptor must contend on the same inode");
        assert!(
            is_lock_contention(&blocked),
            "blocked by the holder: {blocked}"
        );

        // Simulate the holder dying: its descriptor closes, the kernel
        // releases the lock.
        drop(holder);
        try_lock_exclusive(&contender).expect("kernel releases the lock on close");
        drop(contender);
    }

    /// A lock file left behind by a crashed owner (stale owner record, kernel
    /// lock long gone) is immediately acquirable — recovery needs no pid/age
    /// heuristic because the kernel released the lock when the owner died.
    #[test]
    fn process_file_lock_from_a_crashed_owner_is_immediately_acquirable() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("overrides.lock");
        // Simulate a crashed owner's leftover: an owner record for a dead pid.
        std::fs::write(&lock_path, b"pid=4294967295\nacquired_at_ms=1\n").unwrap();
        let acquired = ProcessFileLock::acquire(&lock_path, Duration::from_secs(5))
            .expect("a crashed owner's lock is immediately recoverable");
        drop(acquired);
    }

    /// Releasing a guard cannot affect a successor: the file is never unlinked,
    /// so every successor locks the same inode and each release is independent.
    #[test]
    fn process_file_lock_release_cannot_affect_a_successor() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("overrides.lock");

        let first = ProcessFileLock::acquire(&lock_path, Duration::from_secs(5))
            .expect("first acquirer wins");
        drop(first);
        let second = ProcessFileLock::acquire(&lock_path, Duration::from_secs(5))
            .expect("successor acquires the same file");
        drop(second);
        let third = ProcessFileLock::acquire(&lock_path, Duration::from_secs(5))
            .expect("a later acquirer still succeeds on the same inode");
        drop(third);

        assert!(
            lock_path.exists(),
            "the lock file survives every acquire/release cycle"
        );
    }
}
