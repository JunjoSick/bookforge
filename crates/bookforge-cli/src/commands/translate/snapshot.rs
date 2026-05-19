use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

use bookforge_core::{
    GlossaryTerm, ResolvedRunSettings, ResolvedRunSettingsSnapshot, RunConfigSnapshot,
};
use bookforge_store::{JobRecord, JobStore};
use sha2::{Digest, Sha256};

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
    glossary_fingerprint: &str,
    glossary_terms: &[GlossaryTerm],
    model: &str,
    base_url: Option<String>,
    api_key_env: Option<String>,
) -> anyhow::Result<RunConfigSnapshot> {
    let reports = report_paths(output);
    let events_path = cli_args
        .progress_jsonl
        .clone()
        .unwrap_or_else(|| default_event_path(&job.id));
    let input_snapshot = snapshot_input_epub(store, job, input)?;
    let snapshot = RunConfigSnapshot {
        input_path: input.to_path_buf(),
        input_snapshot_path: Some(input_snapshot.epub_path.clone()),
        input_sha256: Some(input_snapshot.sha256.clone()),
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
        book_id: cli_args.book_id.clone(),
        series_id: cli_args.series_id.clone(),
        glossary_budget_tokens: cli_args.glossary_budget_tokens,
        glossary_format: cli_args.glossary_format,
        prompt_extra: cli_args.prompt_extra.clone(),
        glossary_fingerprint: glossary_fingerprint.to_string(),
        glossary_terms: glossary_terms.to_vec(),
        context_window: cli_args.context_window,
        context_budget_tokens: cli_args.context_budget_tokens,
        context_scope: cli_args.context_scope,
        settings: ResolvedRunSettingsSnapshot::from_settings(settings),
    };
    store.update_job_config_snapshot(&job.id, &snapshot)?;
    store.update_job_event_path(&job.id, &events_path)?;
    Ok(snapshot)
}

#[derive(Debug, Clone)]
struct InputSnapshot {
    epub_path: PathBuf,
    sha256: String,
}

fn snapshot_input_epub(
    store: &JobStore,
    job: &JobRecord,
    input: &Path,
) -> anyhow::Result<InputSnapshot> {
    let run_dir = PathBuf::from(".bookforge/runs").join(&job.id);
    fs::create_dir_all(&run_dir)?;
    let epub_path = run_dir.join("input.epub");
    let sha_path = run_dir.join("input.sha256");

    let sha256 = match fs::hard_link(input, &epub_path) {
        Ok(()) => sha256_file(&epub_path)?,
        Err(_) => copy_and_hash(input, &epub_path)?,
    };

    fs::write(&sha_path, format!("{sha256}\n"))?;
    store.update_job_input_snapshot(&job.id, &epub_path, &sha256)?;

    Ok(InputSnapshot { epub_path, sha256 })
}

fn copy_and_hash(input: &Path, output: &Path) -> anyhow::Result<String> {
    let mut reader = fs::File::open(input)?;
    let mut writer = fs::File::create(output)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];

    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        writer.write_all(&buffer[..read])?;
    }
    writer.flush()?;
    Ok(hex_digest(hasher.finalize().as_slice()))
}

fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let mut reader = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex_digest(hasher.finalize().as_slice()))
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to string cannot fail");
    }
    output
}
