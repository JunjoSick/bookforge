//! Cross-process exclusion for the audiobook output directory (AUDIO-2).
//!
//! Concurrent builds sharing one `out_dir` interleave their
//! `manifest.json` checkpoints and make `--prune` delete the other run's
//! freshly paid chunks. The builder therefore holds one advisory lock for
//! the entire lifetime of a build.
//!
//! The repo has no flock/fs2-style dependency available in-tree, so this is
//! a std-only exclusive-create protocol with explicit ownership metadata:
//!
//! - Acquire: atomically `create_new` `.bookforge-audio.lock` inside the
//!   output directory, then write the owner record (pid + start time). If
//!   creation loses to an existing lock, the file is read and, on Linux,
//!   a provably dead owner pid is reclaimed; otherwise acquisition fails
//!   with a [`LockError::Held`] naming the owning run.
//! - Hold: the returned guard keeps the protocol stateless afterward — any
//!   second acquirer sees `AlreadyExists` while the first run lives.
//! - Release: dropping the guard removes the file *only after re-reading it
//!   and confirming its recorded pid is ours*, so a lock reclaimed or
//!   replaced by another process is never deleted on our behalf.
//!
//! PID liveness is checked through procfs where it exists (Linux); on other
//! platforms every existing owner counts as live and stale locks must be
//! removed manually — the error message says exactly which file to check.
//! This favors safety over convenience: a false "dead" verdict would let a
//! second run corrupt the first one's manifest.

use std::fmt;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Lock file convention inside every audiobook out_dir. Prune sweeps treat
/// the name as protected: a live build's lock file is never deleted.
pub(crate) const LOCK_FILE_NAME: &str = ".bookforge-audio.lock";

const ACQUIRE_ATTEMPTS: usize = 3;
const RETRY_DELAY: Duration = Duration::from_millis(50);

#[derive(Debug)]
pub(crate) struct OutDirLock {
    path: PathBuf,
    /// Process that wrote the record; exposed for tests and diagnostics.
    pub(crate) pid: u32,
}

impl fmt::Display for OutDirLock {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} held by pid {}",
            self.path.display(),
            self.pid
        )
    }
}

impl Drop for OutDirLock {
    fn drop(&mut self) {
        // Verify ownership before removing: another process may have
        // reclaimed our lock after proving us dead, and deleting its file
        // would defeat the whole protocol.
        if let Ok(record) = read_lock_record(&self.path)
            && record.pid == self.pid
        {
            let _ = std::fs::remove_file(&self.path);
        }
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
                 Wait for that run to finish, or verify it is dead and remove the lock file",
                self.lock_path.display(),
                holder.pid,
                holder.started_at_ms
            ),
            None => write!(
                formatter,
                "'{}' is locked, but its owner record could not be read. \
                 No live BookForge audiobook run can be identified; verify no build \
                 is active and remove the lock file to continue",
                self.lock_path.display()
            ),
        }
    }
}

/// Serialized owner payload. Text, not JSON, so an operator can inspect a
/// stuck lock with plain `cat`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerRecord {
    pub pid: u32,
    pub started_at_ms: u64,
}

fn parse_owner_record(contents: &str) -> Option<OwnerRecord> {
    let mut pid = None;
    let mut started_at_ms = None;
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
    }
    Some(OwnerRecord {
        pid: pid?,
        started_at_ms: started_at_ms.unwrap_or(0),
    })
}

fn read_lock_record(path: &Path) -> std::io::Result<OwnerRecord> {
    let mut file = std::fs::File::open(path)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    Ok(parse_owner_record(&contents).unwrap_or(OwnerRecord {
        pid: 0,
        started_at_ms: 0,
    }))
}

/// Best-effort liveness probe. Linux reads procfs; everywhere else the
/// conservative answer "alive" keeps reclaim disabled.
fn process_is_live(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        Path::new("/proc").join(pid.to_string()).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        true
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

/// Try once to take the lock via exclusive creation, then claim it.
fn try_acquire(path: &Path) -> std::io::Result<std::fs::File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

/// Acquire the process-lifetime build lock for `out_dir`, retrying briefly
/// around a reclaimed dead-owner race so two simultaneous reclaimers do not
/// spuriously fail each other.
pub(crate) fn acquire_out_dir_lock(out_dir: &Path) -> Result<OutDirLock, LockError> {
    let path = out_dir.join(LOCK_FILE_NAME);
    let mut last_seen_holder = None;
    for _attempt in 0..ACQUIRE_ATTEMPTS {
        match try_acquire(&path) {
            Ok(mut file) => {
                let record = format!("pid={}\nstarted_at_ms={}\n", std::process::id(), now_ms());
                // Claim failure still leaves an empty lock behind whose
                // unreadable record tells the next contender nobody owns it;
                // surface the error rather than pretending we hold nothing.
                if let Err(source) = file
                    .write_all(record.as_bytes())
                    .and_then(|()| file.flush())
                {
                    return Err(LockError::Io { path, source });
                }
                return Ok(OutDirLock {
                    path,
                    pid: std::process::id(),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let holder = read_lock_record(&path).ok();
                let reclaimable = holder
                    .as_ref()
                    .is_some_and(|record| record.pid != 0 && !process_is_live(record.pid));
                if reclaimable && std::fs::remove_file(&path).is_ok() {
                    // Loop immediately: whichever contender recreates first
                    // wins; the other sees a live owner or an empty record.
                    continue;
                }
                if !reclaimable {
                    return Err(LockError::Held {
                        detail: HeldDetail {
                            lock_path: path.clone(),
                            holder,
                        },
                    });
                }
                // Reclaim lost the recreate race; bounded backoff, retry.
                last_seen_holder = read_lock_record(&path).ok();
                std::thread::sleep(RETRY_DELAY);
            }
            Err(source) => {
                return Err(LockError::Io { path, source });
            }
        }
    }
    Err(LockError::Held {
        detail: HeldDetail {
            lock_path: path.clone(),
            holder: last_seen_holder.or_else(|| read_lock_record(&path).ok()),
        },
    })
}
