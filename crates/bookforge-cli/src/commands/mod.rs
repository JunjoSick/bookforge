pub mod audiobook;
pub mod control;
pub mod convert;
pub mod correct;
pub mod doctor;
pub mod entity;
pub mod estimate;
pub mod glossary;
pub mod ingest_flags;
pub mod inspect;
pub(crate) mod output;
pub mod plan;
pub mod reconfigure;
pub mod reflow;
pub mod resume;
pub mod retry;
pub mod review;
#[cfg(feature = "serve")]
pub mod serve;
pub mod status;
pub mod style;
pub mod tail;
pub mod translate;
pub mod validate;
#[cfg(feature = "tui")]
pub mod watch;

/// Lowest accepted dashboard refresh interval in milliseconds. `watch` and
/// `serve` previously enforced different floors (20 vs 50); both now share
/// this one so a flag value behaves identically on either UI.
#[cfg(any(feature = "serve", feature = "tui"))]
pub(crate) const MIN_REFRESH_MS: u64 = 20;

pub(crate) fn resolve_job_input(
    job: &bookforge_store::JobRecord,
    snapshot: &bookforge_core::RunConfigSnapshot,
) -> anyhow::Result<std::path::PathBuf> {
    if let Some(path) = snapshot
        .input_snapshot_path
        .as_ref()
        .or(job.input_snapshot_path.as_ref())
        && path.exists()
    {
        return Ok(path.clone());
    }

    if snapshot.input_snapshot_path.is_none() && job.input_snapshot_path.is_none() {
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
        );
    }

    let snapshot_path = snapshot
        .input_snapshot_path
        .as_ref()
        .or(job.input_snapshot_path.as_ref())
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "<missing>".to_string());
    anyhow::bail!(
        "job '{}' input snapshot is missing: {}",
        job.id,
        snapshot_path
    )
}
