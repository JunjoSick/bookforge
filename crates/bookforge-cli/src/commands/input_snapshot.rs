//! Saved-input resolution for commands that reopen a job's source EPUB
//! (`resume`, `review`, `correct`).
//!
//! `translate` copies the input into the job's run directory and records its
//! SHA-256. Every later command reads that private copy, and checks it against
//! the recorded checksum before trusting it: a changed snapshot can keep the
//! same segment IDs and text while its assets differ, so checkpoint
//! compatibility alone cannot detect it. Jobs that predate snapshots fall back
//! to the original input path; a job without a recorded checksum is accepted
//! as it is rather than given a new baseline.

use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use bookforge_core::RunConfigSnapshot;
use bookforge_store::JobRecord;
use sha2::{Digest, Sha256};

pub(crate) fn resolve_job_input(job: &JobRecord, snapshot: &RunConfigSnapshot) -> Result<PathBuf> {
    let path = saved_input_path(job, snapshot)?;
    if let Some(expected) = snapshot
        .input_sha256
        .as_deref()
        .or(job.input_sha256.as_deref())
    {
        let actual = sha256_file(&path).with_context(|| {
            format!(
                "checking job '{}' input integrity at {}",
                job.id,
                path.display()
            )
        })?;
        if !actual.eq_ignore_ascii_case(expected.trim()) {
            anyhow::bail!(
                "job '{}' input snapshot SHA-256 mismatch at {}: expected {}, found {}. \
                 Restore the saved input or start a new translation; existing checkpoints \
                 have not been changed",
                job.id,
                path.display(),
                expected.trim(),
                actual
            );
        }
    }
    Ok(path)
}

fn saved_input_path(job: &JobRecord, snapshot: &RunConfigSnapshot) -> Result<PathBuf> {
    let recorded = snapshot
        .input_snapshot_path
        .as_ref()
        .or(job.input_snapshot_path.as_ref());
    if let Some(path) = recorded {
        if path.exists() {
            return Ok(path.clone());
        }
        anyhow::bail!(
            "job '{}' input snapshot is missing: {}",
            job.id,
            path.display()
        );
    }
    tracing::warn!(
        "job '{}' predates input EPUB snapshots; falling back to original input path",
        job.id
    );
    if snapshot.input_path.exists() {
        return Ok(snapshot.input_path.clone());
    }
    anyhow::bail!(
        "job '{}' does not have an input snapshot and the original input path no longer exists: {}",
        job.id,
        snapshot.input_path.display()
    )
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = match file.read(&mut buffer) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}
