//! Cross-process exclusion for the audiobook output directory (AUDIO-2).
//!
//! Concurrent builds sharing one `out_dir` interleave their
//! `manifest.json` checkpoints and make `--prune` delete the other run's
//! freshly paid chunks. The builder therefore holds one advisory lock for
//! the entire lifetime of a build.
//!
//! The gate is a kernel-backed advisory lock held for the whole build:
//! `flock(2)` on Unix and `LockFileEx` on Windows, both exclusive, taken on a
//! `.bookforge-audio.lock` file that also carries a human-readable owner
//! record (pid + start time + nonce) for diagnostics and for the dashboard's
//! spawn handoff. Because the kernel releases the lock automatically when the
//! holder exits, there is no stale-owner reclaim and no unlink race: a
//! crashed build never leaves a permanently held lock, and nothing ever
//! deletes or recreates the lock file, so the kernel lock cannot be split
//! across inodes.
//!
//! # Ownership handoff across a spawn
//!
//! The dashboard parent holds the kernel lock, records a fresh nonce for the
//! child it is about to launch, spawns the child (passing the nonce in the
//! child's environment), then releases the kernel lock. The child waits on
//! the same kernel lock (bounded), acquires it, and — only after verifying
//! the record still carries that nonce — adopts the lock by rewriting the
//! record to name itself. An unrelated waiter that acquires the lock but is
//! not the addressed child releases it and fails closed. No builder ever
//! works without holding the kernel lock, so two processes can never
//! double-spend or prune each other.
//!
//! Terminal writers (the dashboard watcher and restart cancellation) also
//! acquire the kernel lock before touching state, compare the recorded
//! nonce/PID against the child they are closing out, and release without
//! writing if a newer owner took over.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Lock file convention inside every audiobook out_dir. Prune sweeps treat
/// the name as protected: it is never deleted or recreated so the kernel lock
/// cannot be split across inodes.
pub(crate) const LOCK_FILE_NAME: &str = ".bookforge-audio.lock";

/// Bounded wait for a handoff child on a contended kernel lock. The parent
/// holds it only across a spawn handoff, so 30s is generous even under load.
const HANDOFF_WAIT: Duration = Duration::from_secs(30);
const LOCK_POLL: Duration = Duration::from_millis(25);
static RECORD_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Kernel advisory-lock primitives: exclusive `flock` on Unix and exclusive
/// `LockFileEx` on Windows, with a non-blocking variant for fail-fast
/// acquisitions and a bounded waiting variant for handoff children.
#[cfg(unix)]
mod kernel_lock {
    use std::fs::File;
    use std::io;
    use std::os::unix::io::AsRawFd;
    use std::time::{Duration, Instant};

    use super::LOCK_POLL;

    unsafe extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }
    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;
    const LOCK_UN: i32 = 8;

    pub fn try_lock(file: &File) -> io::Result<()> {
        let rc = unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub fn lock_waiting(file: &File, timeout: Duration) -> io::Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            match try_lock(file) {
                Ok(()) => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "timed out waiting for the audiobook output lock",
                        ));
                    }
                    std::thread::sleep(LOCK_POLL);
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub fn unlock(file: &File) {
        unsafe {
            flock(file.as_raw_fd(), LOCK_UN);
        }
    }
}

#[cfg(windows)]
mod kernel_lock {
    use std::fs::File;
    use std::io;
    use std::os::windows::io::AsRawHandle;
    use std::time::{Duration, Instant};

    use super::LOCK_POLL;

    const ERROR_LOCK_VIOLATION: i32 = 33;

    #[repr(C)]
    struct Overlapped {
        internal: usize,
        internal_high: usize,
        offset: u32,
        offset_high: u32,
        h_event: *mut std::ffi::c_void,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LockFileEx(
            file: *mut std::ffi::c_void,
            flags: u32,
            reserved: u32,
            bytes_low: u32,
            bytes_high: u32,
            overlapped: *mut Overlapped,
        ) -> i32;
        fn UnlockFileEx(
            file: *mut std::ffi::c_void,
            reserved: u32,
            bytes_low: u32,
            bytes_high: u32,
            overlapped: *mut Overlapped,
        ) -> i32;
    }

    const LOCKFILE_FAIL_IMMEDIATELY: u32 = 0x1;
    const LOCKFILE_EXCLUSIVE_LOCK: u32 = 0x2;
    // Lock the whole file range; the max length covers every record.
    const LOCK_WHOLE_FILE: u32 = u32::MAX;

    fn lock_region(file: &File, flags: u32) -> i32 {
        let mut overlapped = Overlapped {
            internal: 0,
            internal_high: 0,
            offset: 0,
            offset_high: 0,
            h_event: std::ptr::null_mut(),
        };
        unsafe {
            LockFileEx(
                file.as_raw_handle() as *mut _,
                flags,
                0,
                LOCK_WHOLE_FILE,
                LOCK_WHOLE_FILE,
                &mut overlapped,
            )
        }
    }

    pub fn try_lock(file: &File) -> io::Result<()> {
        if lock_region(file, LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY) != 0 {
            Ok(())
        } else {
            let error = io::Error::last_os_error();
            // With valid parameters the only realistic failure is contention.
            if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION) {
                Err(io::Error::from(io::ErrorKind::WouldBlock))
            } else {
                Err(error)
            }
        }
    }

    pub fn lock_waiting(file: &File, timeout: Duration) -> io::Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            match try_lock(file) {
                Ok(()) => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "timed out waiting for the audiobook output lock",
                        ));
                    }
                    std::thread::sleep(LOCK_POLL);
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub fn unlock(file: &File) {
        let mut overlapped = Overlapped {
            internal: 0,
            internal_high: 0,
            offset: 0,
            offset_high: 0,
            h_event: std::ptr::null_mut(),
        };
        unsafe {
            UnlockFileEx(
                file.as_raw_handle() as *mut _,
                0,
                LOCK_WHOLE_FILE,
                LOCK_WHOLE_FILE,
                &mut overlapped,
            );
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod kernel_lock {
    use std::fs::File;
    use std::io;
    use std::time::Duration;

    pub fn try_lock(_file: &File) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "kernel file locking is unsupported on this platform",
        ))
    }

    pub fn lock_waiting(_file: &File, _timeout: Duration) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "kernel file locking is unsupported on this platform",
        ))
    }

    pub fn unlock(_file: &File) {}
}

#[derive(Debug)]
pub(crate) struct OutDirLock {
    /// Held open for the whole build; its kernel lock is the ownership gate.
    file: File,
    pub(crate) path: PathBuf,
}

impl OutDirLock {
    /// Read the owner record through the already-locked handle. Callers must
    /// hold the kernel lock so the record is stable; all I/O happens on
    /// `self.file` because Windows byte-range locks reject overlapping I/O
    /// performed through a second handle to the same file.
    pub(crate) fn record(&self) -> std::io::Result<OwnerRecord> {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = &self.file;
        file.seek(SeekFrom::Start(0))?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        Ok(parse_owner_record(&contents).unwrap_or(OwnerRecord {
            pid: 0,
            started_at_ms: 0,
            nonce: None,
        }))
    }

    /// Overwrite the owner record through the already-locked handle. Callers
    /// must hold the kernel lock, so the write is exclusive and the read-verify
    /// then rewrite performed by an adopting child is serialized by the kernel
    /// gate (never a read-then-unconditional-replace race). Used to claim a
    /// fresh record, to hand a nonce to a child, and by an adopting child to
    /// name itself.
    pub(crate) fn write_record(&self, pid: u32, nonce: &str) -> std::io::Result<()> {
        use std::io::{Seek, SeekFrom, Write};
        let mut file = &self.file;
        file.seek(SeekFrom::Start(0))?;
        file.set_len(0)?;
        file.write_all(owner_record_string(pid, now_ms(), nonce).as_bytes())?;
        file.sync_all()
    }
}

impl Drop for OutDirLock {
    fn drop(&mut self) {
        kernel_lock::unlock(&self.file);
        // The file handle closes immediately after, releasing the lock again
        // and leaving the record in place for the next acquirer.
    }
}

impl fmt::Display for OutDirLock {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.path.display())
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum LockError {
    #[error("{detail}")]
    Held { detail: HeldDetail },

    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Explanation attached to a failed acquisition; rendered into build errors
/// verbatim so operators can act without reading source.
#[derive(Debug)]
pub struct HeldDetail {
    pub lock_path: PathBuf,
    pub holder: Option<OwnerRecord>,
}

impl fmt::Display for HeldDetail {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.holder {
            Some(holder) => write!(
                formatter,
                "'{}' is locked by another audiobook run (pid {}, started {} ms since epoch). \
                 Wait for that run to finish",
                self.lock_path.display(),
                holder.pid,
                holder.started_at_ms
            ),
            None => write!(
                formatter,
                "'{}' is locked, but its owner record could not be read. \
                 Wait for the active run to finish and retry",
                self.lock_path.display()
            ),
        }
    }
}

/// Serialized owner payload. Text, not JSON, so an operator can inspect a
/// lock with plain `cat`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerRecord {
    pub pid: u32,
    pub started_at_ms: u64,
    /// Ownership token for the dashboard handoff. `None` for records written
    /// by direct CLI runs or before the nonce existed.
    pub nonce: Option<String>,
}

fn parse_owner_record(contents: &str) -> Option<OwnerRecord> {
    let mut pid = None;
    let mut started_at_ms = None;
    let mut nonce = None;
    for line in contents.lines() {
        if let Some(value) = line.strip_prefix("pid=")
            && let Ok(parsed) = value.trim().parse()
        {
            pid = Some(parsed);
        }
        if let Some(value) = line.strip_prefix("started_at_ms=")
            && let Ok(parsed) = value.trim().parse()
        {
            started_at_ms = Some(parsed);
        }
        if let Some(value) = line.strip_prefix("nonce=") {
            nonce = Some(value.trim().to_string());
        }
    }
    Some(OwnerRecord {
        pid: pid?,
        started_at_ms: started_at_ms.unwrap_or(0),
        nonce,
    })
}

pub(crate) fn read_lock_record(path: &Path) -> std::io::Result<OwnerRecord> {
    let mut file = std::fs::File::open(path)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    Ok(parse_owner_record(&contents).unwrap_or(OwnerRecord {
        pid: 0,
        started_at_ms: 0,
        nonce: None,
    }))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

/// A fresh, unguessable-per-process ownership token for a lock record.
pub(crate) fn generate_nonce() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(std::process::id() as u64);
    hasher.write_u64(now_ms());
    hasher.write_u64(RECORD_SEQUENCE.fetch_add(1, Ordering::Relaxed));
    format!("{:016x}{:016x}", hasher.finish(), now_ms())
}

fn owner_record_string(pid: u32, started_at_ms: u64, nonce: &str) -> String {
    format!("pid={pid}\nstarted_at_ms={started_at_ms}\nnonce={nonce}\n")
}

fn open_lock_file(path: &Path) -> std::io::Result<File> {
    // Never truncate on open: the owner record must persist across the
    // parent/child handoff and across the whole life of the out_dir.
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

fn held(path: PathBuf) -> LockError {
    LockError::Held {
        detail: HeldDetail {
            lock_path: path.clone(),
            holder: read_lock_record(&path).ok(),
        },
    }
}

/// Acquire the process-lifetime build lock for `out_dir`, taking the kernel
/// lock non-blockingly and claiming the owner record. Fails immediately with
/// [`LockError::Held`] when another live run holds it; there is no stale
/// reclaim because the kernel releases the lock when a holder dies.
pub(crate) fn acquire_out_dir_lock(out_dir: &Path) -> Result<OutDirLock, LockError> {
    let path = out_dir.join(LOCK_FILE_NAME);
    let file = open_lock_file(&path).map_err(|source| LockError::Io {
        path: path.clone(),
        source,
    })?;
    if let Err(source) = kernel_lock::try_lock(&file) {
        if source.kind() == std::io::ErrorKind::WouldBlock {
            return Err(held(path));
        }
        return Err(LockError::Io { path, source });
    }
    let lock = OutDirLock {
        file,
        path: path.clone(),
    };
    let nonce = generate_nonce();
    if let Err(source) = lock.write_record(std::process::id(), &nonce) {
        drop(lock);
        return Err(LockError::Io { path, source });
    }
    Ok(lock)
}

/// Acquire the kernel lock waiting for a dashboard parent to release it after
/// spawning this child (bounded by [`HANDOFF_WAIT`]), then adopt the lock only
/// if the record still carries `handoff_nonce`. An unrelated waiter that wins
/// the lock but is not the addressed child releases it and fails closed.
pub(crate) fn acquire_out_dir_lock_with_handoff(
    out_dir: &Path,
    handoff_nonce: &str,
) -> Result<OutDirLock, LockError> {
    let path = out_dir.join(LOCK_FILE_NAME);
    let file = open_lock_file(&path).map_err(|source| LockError::Io {
        path: path.clone(),
        source,
    })?;
    if let Err(source) = kernel_lock::lock_waiting(&file, HANDOFF_WAIT) {
        return Err(if source.kind() == std::io::ErrorKind::WouldBlock {
            held(path)
        } else {
            LockError::Io { path, source }
        });
    }
    let lock = OutDirLock {
        file,
        path: path.clone(),
    };
    let record = match lock.record() {
        Ok(record) => record,
        Err(source) => {
            drop(lock);
            return Err(LockError::Io {
                path: path.clone(),
                source,
            });
        }
    };
    if record.nonce.as_deref() != Some(handoff_nonce) {
        // Not the addressed child (a newer owner took over or the parent's
        // record is gone): release the kernel lock and fail closed rather
        // than overwrite the record.
        drop(lock);
        return Err(LockError::Held {
            detail: HeldDetail {
                lock_path: path.clone(),
                holder: Some(record),
            },
        });
    }
    if let Err(source) = lock.write_record(std::process::id(), handoff_nonce) {
        drop(lock);
        return Err(LockError::Io {
            path: path.clone(),
            source,
        });
    }
    Ok(lock)
}

/// Take the kernel lock non-blockingly WITHOUT claiming the owner record, for
/// terminal writers that must first inspect who currently owns the lock. Fails
/// with [`LockError::Held`] when another live run holds it.
pub(crate) fn acquire_out_dir_lock_peek(out_dir: &Path) -> Result<OutDirLock, LockError> {
    let path = out_dir.join(LOCK_FILE_NAME);
    let file = open_lock_file(&path).map_err(|source| LockError::Io {
        path: path.clone(),
        source,
    })?;
    if let Err(source) = kernel_lock::try_lock(&file) {
        if source.kind() == std::io::ErrorKind::WouldBlock {
            return Err(held(path));
        }
        return Err(LockError::Io { path, source });
    }
    Ok(OutDirLock { file, path })
}
