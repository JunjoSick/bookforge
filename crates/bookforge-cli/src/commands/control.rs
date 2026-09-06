use anyhow::Result;
use bookforge_core::ControlCommand;
use bookforge_store::{JobRecord, JobStore};
use clap::Args;

#[derive(Debug, Args)]
pub struct PauseArgs {
    /// Job to pause. The worker checkpoints completed work before parking.
    pub job_id: String,
}

#[derive(Debug, Args)]
pub struct StopArgs {
    /// Job to stop. Completed work remains resumable from its checkpoints.
    pub job_id: String,
}

pub async fn pause(args: PauseArgs) -> Result<()> {
    let store = JobStore::open_default()?;
    ensure_job_exists(&store, &args.job_id)?;
    let path = crate::control::request_job_control(&args.job_id, ControlCommand::Pause)?;
    println!("pause requested for {} ({})", args.job_id, path.display());
    Ok(())
}

pub async fn stop(args: StopArgs) -> Result<()> {
    let store = JobStore::open_default()?;
    ensure_job_exists(&store, &args.job_id)?;
    let path = crate::control::request_job_control(&args.job_id, ControlCommand::Stop)?;
    println!("stop requested for {} ({})", args.job_id, path.display());
    Ok(())
}

/// Refuse to write control files for jobs that do not exist: a typo'd ID used
/// to be accepted silently and the pause/stop was simply never observed (CLI-14).
fn ensure_job_exists(store: &JobStore, job_id: &str) -> Result<JobRecord> {
    store
        .get_job(job_id)?
        .ok_or_else(|| anyhow::anyhow!("job '{job_id}' does not exist; no control command sent"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookforge_store::CreateJob;

    #[test]
    fn pause_rejects_typod_job_ids_without_writing_control() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("jobs.sqlite");
        std::fs::create_dir_all(dir.path()).unwrap();
        let store = JobStore::open(&db).unwrap();

        let error = ensure_job_exists(&store, "job_typo_does_not_exist")
            .expect_err("typo'd ids must fail loudly");

        assert!(error.to_string().contains("job_typo_does_not_exist"));
        assert!(error.to_string().contains("does not exist"));
        let control_path = bookforge_core::control_path_for_job("job_typo_does_not_exist");
        assert!(
            !control_path.exists(),
            "no control file may be written for a nonexistent job"
        );
    }

    #[test]
    fn pause_accepts_existing_job_ids() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input.epub");
        std::fs::write(&input, b"epub").unwrap();
        let store = JobStore::open(dir.path().join("jobs.sqlite")).unwrap();
        let job = store
            .create_job(CreateJob {
                input: &input,
                output: &dir.path().join("out.epub"),
                source_lang: Some("English"),
                target_lang: "Italian",
                provider: "mock",
                model: "mock-prefix-target",
                base_url: None,
                api_key_env: None,
                book_id: None,
                series_id: None,
            })
            .unwrap();

        let record = ensure_job_exists(&store, &job.id).expect("existing job resolves");
        assert_eq!(record.id, job.id);
    }
}
