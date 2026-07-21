use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

use bookforge_core::{
    FallbackRunConfigSnapshot, FinalizeCheckpointSnapshot, GlossaryTerm, ResolvedRunSettings,
    ResolvedRunSettingsSnapshot, RunConfigSnapshot,
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
    style_fingerprint: &str,
    style_rendered_block: &str,
    entities_fingerprint: &str,
    entities_rendered_block: &str,
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
    let bilingual_css = read_bilingual_css(cli_args)?;
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
        creator: cli_args.creator.clone(),
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
        style_fingerprint: style_fingerprint.to_string(),
        style_rendered_block: style_rendered_block.to_string(),
        entities_fingerprint: entities_fingerprint.to_string(),
        entities_rendered_block: entities_rendered_block.to_string(),
        bilingual_mode: cli_args.mode,
        bilingual_separator: cli_args.bilingual_separator.clone(),
        bilingual_style: cli_args.bilingual_style,
        bilingual_css,
        fallback: fallback_snapshot(cli_args, model),
        finalize: FinalizeCheckpointSnapshot::default(),
        qa_mode: cli_args.qa.as_str().to_string(),
        validate_output: cli_args.validate_output,
        settings: ResolvedRunSettingsSnapshot::from_settings(settings),
    };
    store.update_job_config_snapshot(&job.id, &snapshot)?;
    store.update_job_event_path(&job.id, &events_path)?;
    Ok(snapshot)
}

fn fallback_snapshot(
    cli_args: &TranslateArgs,
    primary_model: &str,
) -> Option<FallbackRunConfigSnapshot> {
    if cli_args.fallback_provider.is_none() && cli_args.fallback_model.is_none() {
        return None;
    }
    Some(FallbackRunConfigSnapshot {
        provider: cli_args
            .fallback_provider
            .clone()
            .unwrap_or_else(|| "openrouter".to_string()),
        model: cli_args
            .fallback_model
            .clone()
            .unwrap_or_else(|| primary_model.to_string()),
        base_url: cli_args.fallback_base_url.clone(),
        api_key_env: cli_args.fallback_api_key_env.clone(),
        scope: cli_args.fallback_only,
    })
}

fn read_bilingual_css(cli_args: &TranslateArgs) -> anyhow::Result<Option<String>> {
    let Some(path) = cli_args.bilingual_css.as_ref() else {
        return Ok(None);
    };
    std::fs::read_to_string(path)
        .map(Some)
        .map_err(|err| anyhow::anyhow!("failed to read --bilingual-css {}: {err}", path.display()))
}

#[derive(Debug, Clone)]
struct InputSnapshot {
    epub_path: PathBuf,
    sha256: String,
}

#[cfg(unix)]
fn create_private_dir_all(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_private_dir_all(path: &Path) -> std::io::Result<()> {
    fs::create_dir_all(path)
}

fn snapshot_input_epub(
    store: &JobStore,
    job: &JobRecord,
    input: &Path,
) -> anyhow::Result<InputSnapshot> {
    let run_dir = PathBuf::from(".bookforge/runs").join(&job.id);
    create_private_dir_all(&run_dir)?;
    let epub_path = run_dir.join("input.epub");
    let sha_path = run_dir.join("input.sha256");

    // A hard link shares its mode with the source inode, so Unix must copy in
    // order to make the private snapshot 0600 without modifying the input.
    #[cfg(unix)]
    let sha256 = copy_and_hash(input, &epub_path)?;
    #[cfg(not(unix))]
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
    let mut writer = create_private_snapshot_file(output)?;
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

#[cfg(unix)]
fn create_private_snapshot_file(path: &Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    // The creation mode is already private; normalize owner bits in case the
    // process has an unusually restrictive umask.
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

#[cfg(not(unix))]
fn create_private_snapshot_file(path: &Path) -> std::io::Result<fs::File> {
    fs::File::create(path)
}

#[cfg(not(unix))]
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

#[cfg(all(test, unix))]
mod unix_permissions_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn snapshot_directory_and_file_are_private() {
        let root = tempfile::tempdir().expect("test directory should be created");
        let run_dir = root.path().join(".bookforge/runs/test-job");
        create_private_dir_all(&run_dir).expect("run directory should be created");
        let input = root.path().join("source.epub");
        fs::write(&input, b"private book contents").expect("input should be written");
        let snapshot = run_dir.join("input.epub");

        copy_and_hash(&input, &snapshot).expect("snapshot should be copied");

        for directory in [
            root.path().join(".bookforge"),
            root.path().join(".bookforge/runs"),
            run_dir,
        ] {
            let mode = fs::metadata(&directory)
                .expect("directory metadata should be readable")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700, "unexpected mode for {}", directory.display());
        }
        let snapshot_mode = fs::metadata(&snapshot)
            .expect("snapshot metadata should be readable")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(snapshot_mode, 0o600);
    }
}
