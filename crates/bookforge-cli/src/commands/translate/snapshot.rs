use std::path::{Path, PathBuf};

use bookforge_core::{ResolvedRunSettings, ResolvedRunSettingsSnapshot, RunConfigSnapshot};
use bookforge_store::{JobRecord, JobStore};

use crate::{ProviderArgs as CliProviderArgs, report::report_paths};

use super::args::TranslateArgs;

pub(crate) fn default_event_path(job_id: &str) -> PathBuf {
    PathBuf::from(".bookforge/runs")
        .join(job_id)
        .join("events.jsonl")
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn persist_snapshot(
    store: &JobStore,
    job: &JobRecord,
    input: &Path,
    output: &Path,
    provider_args: &CliProviderArgs,
    cli_args: &TranslateArgs,
    settings: &ResolvedRunSettings,
    prompt_version: &str,
    cache_namespace: &str,
    model: &str,
    base_url: Option<String>,
    api_key_env: Option<String>,
) -> anyhow::Result<RunConfigSnapshot> {
    let reports = report_paths(output);
    let events_path = cli_args
        .progress_jsonl
        .clone()
        .unwrap_or_else(|| default_event_path(&job.id));
    let snapshot = RunConfigSnapshot {
        input_path: input.to_path_buf(),
        output_path: output.to_path_buf(),
        events_path: Some(events_path.clone()),
        report_json_path: Some(reports.json),
        report_markdown_path: Some(reports.markdown),
        source_language: cli_args.language.source.clone(),
        target_language: cli_args.language.target.clone(),
        provider: provider_args.provider.clone(),
        model: model.to_string(),
        base_url,
        api_key_env,
        profile: settings.profile,
        provider_preset: cli_args.provider_preset,
        prompt_version: prompt_version.to_string(),
        cache_namespace: cache_namespace.to_string(),
        settings: ResolvedRunSettingsSnapshot::from_settings(settings),
    };
    store.update_job_config_snapshot(&job.id, &snapshot)?;
    store.update_job_event_path(&job.id, &events_path)?;
    Ok(snapshot)
}
