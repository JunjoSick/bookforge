use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, Result};
use bookforge_core::{
    GlossaryFormat, ProviderPreset, ResolvedRunSettings,
    config::{ContextScope, DoubleCheckMode, TranslationProfile},
    now_ms, run_dir_for_job,
};
use bookforge_store::JobStore;
use clap::Args;

use crate::QaMode;

#[derive(Debug, Args)]
pub struct ReconfigureArgs {
    pub job_id: String,

    #[arg(long)]
    pub batch_max_output_tokens: Option<u32>,

    #[arg(long)]
    pub batch_max_items: Option<usize>,

    #[arg(long)]
    pub batch_target_tokens: Option<usize>,

    #[arg(long)]
    pub concurrency: Option<usize>,

    #[arg(long, value_enum)]
    pub qa: Option<QaMode>,

    #[arg(long, value_enum)]
    pub double_check: Option<DoubleCheckMode>,

    #[arg(long, num_args = 0..=1, default_missing_value = "true")]
    pub validate_output: Option<bool>,

    #[arg(long)]
    pub provider_max_attempts: Option<usize>,

    #[arg(long, num_args = 0..=1, default_missing_value = "true")]
    pub adaptive_concurrency: Option<bool>,

    #[arg(long, num_args = 0..=1, default_missing_value = "true")]
    pub adaptive_batch_sizing: Option<bool>,

    #[arg(long)]
    provider: Option<String>,

    #[arg(long)]
    model: Option<String>,

    #[arg(long)]
    source: Option<String>,

    #[arg(long)]
    target: Option<String>,

    #[arg(long, value_enum)]
    profile: Option<TranslationProfile>,

    #[arg(long)]
    max_segment_tokens: Option<usize>,

    #[arg(long)]
    context_tokens: Option<usize>,

    #[arg(long)]
    context_window: Option<usize>,

    #[arg(long)]
    context_budget_tokens: Option<usize>,

    #[arg(long, value_enum)]
    context_scope: Option<ContextScope>,

    #[arg(long)]
    prompt_version: Option<String>,

    #[arg(long = "glossary")]
    glossary: Vec<PathBuf>,

    #[arg(long)]
    glossary_budget_tokens: Option<usize>,

    #[arg(long, value_enum)]
    glossary_format: Option<GlossaryFormat>,

    #[arg(long)]
    prompt_extra: Option<String>,

    #[arg(long = "style")]
    style: Vec<PathBuf>,

    #[arg(long = "entities")]
    entities: Vec<PathBuf>,

    #[arg(long, value_enum)]
    provider_preset: Option<ProviderPreset>,
}

const OVERRIDES_SCHEMA_VERSION: u32 = 1;
const OVERRIDES_LOCK_WAIT: Duration = Duration::from_secs(5);
const OVERRIDES_STALE_LOCK_AGE: Duration = Duration::from_secs(30);
static OVERRIDES_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct RunConfigOverrides {
    pub batch_max_output_tokens: Option<u32>,
    pub batch_max_items: Option<usize>,
    pub batch_target_tokens: Option<usize>,
    pub concurrency: Option<usize>,
    pub qa: Option<QaMode>,
    pub double_check: Option<DoubleCheckMode>,
    pub validate_output: Option<bool>,
    pub provider_max_attempts: Option<usize>,
    pub adaptive_concurrency: Option<bool>,
    pub adaptive_batch_sizing: Option<bool>,
}

impl RunConfigOverrides {
    fn from_args(args: &ReconfigureArgs) -> Self {
        Self {
            batch_max_output_tokens: args.batch_max_output_tokens,
            batch_max_items: args.batch_max_items,
            batch_target_tokens: args.batch_target_tokens,
            concurrency: args.concurrency,
            qa: args.qa,
            double_check: args.double_check,
            validate_output: args.validate_output,
            provider_max_attempts: args.provider_max_attempts,
            adaptive_concurrency: args.adaptive_concurrency,
            adaptive_batch_sizing: args.adaptive_batch_sizing,
        }
    }

    fn merge(self, existing: Self) -> Self {
        Self {
            batch_max_output_tokens: self
                .batch_max_output_tokens
                .or(existing.batch_max_output_tokens),
            batch_max_items: self.batch_max_items.or(existing.batch_max_items),
            batch_target_tokens: self.batch_target_tokens.or(existing.batch_target_tokens),
            concurrency: self.concurrency.or(existing.concurrency),
            qa: self.qa.or(existing.qa),
            double_check: self.double_check.or(existing.double_check),
            validate_output: self.validate_output.or(existing.validate_output),
            provider_max_attempts: self
                .provider_max_attempts
                .or(existing.provider_max_attempts),
            adaptive_concurrency: self.adaptive_concurrency.or(existing.adaptive_concurrency),
            adaptive_batch_sizing: self
                .adaptive_batch_sizing
                .or(existing.adaptive_batch_sizing),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.batch_max_output_tokens.is_none()
            && self.batch_max_items.is_none()
            && self.batch_target_tokens.is_none()
            && self.concurrency.is_none()
            && self.qa.is_none()
            && self.double_check.is_none()
            && self.validate_output.is_none()
            && self.provider_max_attempts.is_none()
            && self.adaptive_concurrency.is_none()
            && self.adaptive_batch_sizing.is_none()
    }

    fn validate(&self) -> Result<()> {
        let mut zero_fields = Vec::new();
        if self.batch_max_output_tokens == Some(0) {
            zero_fields.push("batch-max-output-tokens");
        }
        if self.batch_max_items == Some(0) {
            zero_fields.push("batch-max-items");
        }
        if self.batch_target_tokens == Some(0) {
            zero_fields.push("batch-target-tokens");
        }
        if self.concurrency == Some(0) {
            zero_fields.push("concurrency");
        }
        if self.provider_max_attempts == Some(0) {
            zero_fields.push("provider-max-attempts");
        }
        if zero_fields.is_empty() {
            return Ok(());
        }
        anyhow::bail!(
            "runtime settings must be greater than zero: {}",
            zero_fields.join(", ")
        )
    }

    pub(crate) fn changed_fields(&self) -> Vec<String> {
        let mut fields = Vec::new();
        if self.batch_max_output_tokens.is_some() {
            fields.push("batch-max-output-tokens".to_string());
        }
        if self.batch_max_items.is_some() {
            fields.push("batch-max-items".to_string());
        }
        if self.batch_target_tokens.is_some() {
            fields.push("batch-target-tokens".to_string());
        }
        if self.concurrency.is_some() {
            fields.push("concurrency".to_string());
        }
        if self.qa.is_some() {
            fields.push("qa".to_string());
        }
        if self.double_check.is_some() {
            fields.push("double-check".to_string());
        }
        if self.validate_output.is_some() {
            fields.push("validate-output".to_string());
        }
        if self.provider_max_attempts.is_some() {
            fields.push("provider-max-attempts".to_string());
        }
        if self.adaptive_concurrency.is_some() {
            fields.push("adaptive-concurrency".to_string());
        }
        if self.adaptive_batch_sizing.is_some() {
            fields.push("adaptive-batch-sizing".to_string());
        }
        fields
    }

    pub(crate) fn application_boundaries(&self) -> Vec<String> {
        let mut boundaries = Vec::new();
        if self.concurrency.is_some()
            || self.provider_max_attempts.is_some()
            || self.adaptive_concurrency.is_some()
            || self.batch_max_output_tokens.is_some()
        {
            boundaries.push("next_request".to_string());
        }
        if self.batch_max_items.is_some()
            || self.batch_target_tokens.is_some()
            || self.adaptive_batch_sizing.is_some()
        {
            boundaries.push("next_batch".to_string());
        }
        if self.qa.is_some() || self.double_check.is_some() || self.validate_output.is_some() {
            boundaries.push("next_stage".to_string());
        }
        boundaries
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct OverridesEnvelope {
    schema_version: u32,
    revision: u64,
    updated_at_ms: u64,
    overrides: RunConfigOverrides,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoadedOverrides {
    pub revision: u64,
    pub updated_at_ms: u64,
    pub overrides: RunConfigOverrides,
}

impl LoadedOverrides {
    fn legacy(overrides: RunConfigOverrides) -> Self {
        Self {
            revision: 0,
            updated_at_ms: 0,
            overrides,
        }
    }
}

pub async fn run(args: ReconfigureArgs) -> Result<()> {
    reject_immutable_changes(&args)?;
    let incoming = RunConfigOverrides::from_args(&args);
    if incoming.is_empty() {
        anyhow::bail!(
            "no mutable settings provided; reconfigure accepts cache-safe scheduling, budget, QA, double-check, validation, provider-attempt, and adaptive flags"
        );
    }
    incoming.validate()?;

    let store = JobStore::open_default()?;
    let Some(job) = store.get_job(&args.job_id)? else {
        anyhow::bail!("job '{}' was not found", args.job_id);
    };
    if !matches!(job.status.as_str(), "running" | "paused" | "stopped") {
        anyhow::bail!(
            "job '{}' is '{}'; reconfigure applies only to running, paused, or stopped jobs with remaining work",
            args.job_id,
            job.status
        );
    }
    if store.load_job_config_snapshot(&args.job_id)?.is_none() {
        anyhow::bail!(
            "job '{}' does not have a run configuration snapshot; it cannot be reconfigured safely",
            args.job_id
        );
    }

    let (path, loaded) = write_merged_overrides_for_job(&args.job_id, incoming)?;
    println!("Reconfigured: {}", args.job_id);
    println!("Overrides: {}", path.display());
    println!("Revision: {}", loaded.revision);
    for line in describe_overrides(&loaded.overrides) {
        println!("  {line}");
    }
    println!("Apply: {}", apply_instructions(&args.job_id));
    Ok(())
}

pub(crate) fn apply_overrides_to_settings(
    settings: &mut ResolvedRunSettings,
    overrides: &RunConfigOverrides,
) {
    if let Some(value) = overrides.batch_max_output_tokens {
        settings.provider.batch_max_output_tokens = Some(value);
    }
    if let Some(value) = overrides.batch_max_items {
        settings.batch.max_items = value.max(1);
    }
    if let Some(value) = overrides.batch_target_tokens {
        settings.batch.target_tokens = value.max(1);
    }
    if let Some(value) = overrides.concurrency {
        settings.scheduler.concurrency = value.max(1);
    }
    if let Some(value) = overrides.double_check {
        settings.double_check.mode = value;
    }
    if let Some(value) = overrides.provider_max_attempts {
        settings.provider.provider_max_attempts = value.max(1);
    }
    if let Some(value) = overrides.adaptive_concurrency {
        settings.adaptive_concurrency = value;
    }
    if let Some(value) = overrides.adaptive_batch_sizing {
        settings.batch.adaptive_sizing = value;
    }
}

pub(crate) fn load_overrides_for_job(job_id: &str) -> Result<Option<RunConfigOverrides>> {
    Ok(load_overrides_document_for_job(job_id)?.map(|loaded| loaded.overrides))
}

pub(crate) fn load_overrides_document_for_job(job_id: &str) -> Result<Option<LoadedOverrides>> {
    load_overrides_from_path(&overrides_path_for_job(job_id))
}

pub(crate) fn clear_overrides_for_job(job_id: &str) -> Result<PathBuf> {
    let path = overrides_path_for_job(job_id);
    match fs::remove_file(&path) {
        Ok(()) => Ok(path),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(path),
        Err(err) => Err(err).with_context(|| format!("failed to remove {}", path.display())),
    }
}

pub(crate) fn overrides_path_for_job(job_id: &str) -> PathBuf {
    run_dir_for_job(job_id).join("overrides.json")
}

pub(crate) fn apply_instructions(job_id: &str) -> String {
    format!(
        "A live worker applies this revision at the next safe boundary. If no worker is alive, run `bookforge resume {job_id}` (or `bookforge resume {job_id} --force` for a stale paused status)."
    )
}

pub(crate) fn describe_overrides(overrides: &RunConfigOverrides) -> Vec<String> {
    let mut lines = Vec::new();
    push_opt(
        &mut lines,
        "batch-max-output-tokens",
        overrides.batch_max_output_tokens,
    );
    push_opt(&mut lines, "batch-max-items", overrides.batch_max_items);
    push_opt(
        &mut lines,
        "batch-target-tokens",
        overrides.batch_target_tokens,
    );
    push_opt(&mut lines, "concurrency", overrides.concurrency);
    push_opt(
        &mut lines,
        "qa",
        overrides.qa.map(|value| format!("{value:?}")),
    );
    push_opt(
        &mut lines,
        "double-check",
        overrides.double_check.map(|value| format!("{value:?}")),
    );
    push_opt(&mut lines, "validate-output", overrides.validate_output);
    push_opt(
        &mut lines,
        "provider-max-attempts",
        overrides.provider_max_attempts,
    );
    push_opt(
        &mut lines,
        "adaptive-concurrency",
        overrides.adaptive_concurrency,
    );
    push_opt(
        &mut lines,
        "adaptive-batch-sizing",
        overrides.adaptive_batch_sizing,
    );
    lines
}

fn push_opt<T: ToString>(lines: &mut Vec<String>, name: &str, value: Option<T>) {
    if let Some(value) = value {
        lines.push(format!("{name}: {}", value.to_string()));
    }
}

fn load_overrides_from_path(path: &Path) -> Result<Option<LoadedOverrides>> {
    match fs::read_to_string(path) {
        Ok(contents) => {
            let value: serde_json::Value = serde_json::from_str(&contents)
                .with_context(|| format!("failed to parse {}", path.display()))?;
            let loaded =
                if value.get("schema_version").is_some() || value.get("overrides").is_some() {
                    let envelope: OverridesEnvelope = serde_json::from_value(value)
                        .with_context(|| format!("failed to parse {}", path.display()))?;
                    if envelope.schema_version != OVERRIDES_SCHEMA_VERSION {
                        anyhow::bail!(
                            "unsupported runtime override schema {} in {}; expected {}",
                            envelope.schema_version,
                            path.display(),
                            OVERRIDES_SCHEMA_VERSION
                        );
                    }
                    LoadedOverrides {
                        revision: envelope.revision,
                        updated_at_ms: envelope.updated_at_ms,
                        overrides: envelope.overrides,
                    }
                } else {
                    LoadedOverrides::legacy(
                        serde_json::from_value(value)
                            .with_context(|| format!("failed to parse {}", path.display()))?,
                    )
                };
            loaded.overrides.validate()?;
            Ok(Some(loaded))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
    }
}

pub(crate) fn write_merged_overrides_for_job(
    job_id: &str,
    incoming: RunConfigOverrides,
) -> Result<(PathBuf, LoadedOverrides)> {
    let path = overrides_path_for_job(job_id);
    let loaded = write_merged_overrides_at_path(&path, incoming)?;
    Ok((path, loaded))
}

fn write_merged_overrides_at_path(
    path: &Path,
    incoming: RunConfigOverrides,
) -> Result<LoadedOverrides> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let _lock = OverridesFileLock::acquire(path)?;
    // A reader must retain its last valid in-memory snapshot when the durable
    // document is corrupt. A writer, however, is the recovery mechanism: once
    // it owns the cross-process lock it may replace an unreadable document with
    // a fresh revision-1 envelope instead of making every future edit fail.
    let existing = load_overrides_from_path(path)
        .ok()
        .flatten()
        .unwrap_or_else(|| LoadedOverrides {
            revision: 0,
            updated_at_ms: 0,
            overrides: RunConfigOverrides::default(),
        });
    let overrides = incoming.merge(existing.overrides);
    overrides.validate()?;
    let loaded = LoadedOverrides {
        revision: existing.revision.saturating_add(1),
        updated_at_ms: now_ms(),
        overrides,
    };
    write_overrides_atomically(path, &loaded)?;
    Ok(loaded)
}

fn write_overrides_atomically(path: &Path, loaded: &LoadedOverrides) -> Result<()> {
    let envelope = OverridesEnvelope {
        schema_version: OVERRIDES_SCHEMA_VERSION,
        revision: loaded.revision,
        updated_at_ms: loaded.updated_at_ms,
        overrides: loaded.overrides.clone(),
    };
    let json = serde_json::to_string_pretty(&envelope)?;
    let suffix = OVERRIDES_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("overrides.json");
    let staged = path.with_file_name(format!(
        ".{file_name}.staged-{}-{suffix}",
        std::process::id()
    ));
    let write_result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&staged)
            .with_context(|| format!("failed to create {}", staged.display()))?;
        file.write_all(format!("{json}\n").as_bytes())?;
        file.sync_all()?;
        fs::rename(&staged, path).with_context(|| {
            format!(
                "failed to atomically replace {} with {}",
                path.display(),
                staged.display()
            )
        })?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    write_result
}

struct OverridesFileLock {
    path: PathBuf,
    _file: File,
}

impl OverridesFileLock {
    fn acquire(overrides_path: &Path) -> Result<Self> {
        let path = overrides_path.with_extension("lock");
        let deadline = Instant::now() + OVERRIDES_LOCK_WAIT;
        loop {
            match OpenOptions::new().create_new(true).write(true).open(&path) {
                Ok(mut file) => {
                    writeln!(
                        file,
                        "pid={} acquired_at_ms={}",
                        std::process::id(),
                        now_ms()
                    )?;
                    file.sync_all()?;
                    return Ok(Self { path, _file: file });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if lock_is_stale(&path) {
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                    if Instant::now() >= deadline {
                        anyhow::bail!(
                            "timed out waiting for runtime override lock {}",
                            path.display()
                        );
                    }
                    thread::sleep(Duration::from_millis(25));
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to acquire {}", path.display()));
                }
            }
        }
    }
}

impl Drop for OverridesFileLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn lock_is_stale(path: &Path) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age >= OVERRIDES_STALE_LOCK_AGE)
}

fn reject_immutable_changes(args: &ReconfigureArgs) -> Result<()> {
    let mut rejected = Vec::new();
    if args.provider.is_some() {
        rejected.push("provider");
    }
    if args.model.is_some() {
        rejected.push("model");
    }
    if args.source.is_some() {
        rejected.push("source language");
    }
    if args.target.is_some() {
        rejected.push("target language");
    }
    if args.profile.is_some() {
        rejected.push("profile");
    }
    if args.max_segment_tokens.is_some() {
        rejected.push("max segment tokens");
    }
    if args.context_tokens.is_some()
        || args.context_window.is_some()
        || args.context_budget_tokens.is_some()
        || args.context_scope.is_some()
    {
        rejected.push("context settings");
    }
    if args.prompt_version.is_some() {
        rejected.push("prompt version");
    }
    if !args.glossary.is_empty()
        || args.glossary_budget_tokens.is_some()
        || args.glossary_format.is_some()
        || args.prompt_extra.is_some()
    {
        rejected.push("glossary inputs");
    }
    if !args.style.is_empty() {
        rejected.push("style inputs");
    }
    if !args.entities.is_empty() {
        rejected.push("entity inputs");
    }
    if args.provider_preset.is_some() {
        rejected.push("provider preset");
    }
    if rejected.is_empty() {
        return Ok(());
    }

    anyhow::bail!(
        "cannot reconfigure {} for an existing job; these settings affect cache identity or prompt inputs, so start a fresh run instead",
        rejected.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn immutable_provider_is_rejected_with_fresh_run_message() {
        let args = ReconfigureArgs {
            job_id: "job".to_string(),
            batch_max_output_tokens: None,
            batch_max_items: None,
            batch_target_tokens: None,
            concurrency: None,
            qa: None,
            double_check: None,
            validate_output: None,
            provider_max_attempts: None,
            adaptive_concurrency: None,
            adaptive_batch_sizing: None,
            provider: Some("deepseek".to_string()),
            model: None,
            source: None,
            target: None,
            profile: None,
            max_segment_tokens: None,
            context_tokens: None,
            context_window: None,
            context_budget_tokens: None,
            context_scope: None,
            prompt_version: None,
            glossary: Vec::new(),
            glossary_budget_tokens: None,
            glossary_format: None,
            prompt_extra: None,
            style: Vec::new(),
            entities: Vec::new(),
            provider_preset: None,
        };

        let err = reject_immutable_changes(&args).expect_err("provider must be rejected");

        assert!(err.to_string().contains("provider"));
        assert!(err.to_string().contains("fresh run"));
    }

    #[test]
    fn describes_only_present_overrides() {
        let overrides = RunConfigOverrides {
            batch_max_output_tokens: Some(12_000),
            batch_max_items: Some(3),
            validate_output: Some(true),
            ..RunConfigOverrides::default()
        };

        let lines = describe_overrides(&overrides);

        assert_eq!(
            lines,
            vec![
                "batch-max-output-tokens: 12000",
                "batch-max-items: 3",
                "validate-output: true"
            ]
        );
    }

    #[test]
    fn applies_runtime_overrides_without_touching_cache_fields() {
        let mut settings = TranslationProfile::V1Fast.resolve();
        let overrides = RunConfigOverrides {
            batch_max_output_tokens: Some(12_000),
            batch_max_items: Some(2),
            batch_target_tokens: Some(4_000),
            concurrency: Some(3),
            provider_max_attempts: Some(5),
            adaptive_concurrency: Some(true),
            adaptive_batch_sizing: Some(false),
            double_check: Some(DoubleCheckMode::Semantic),
            ..RunConfigOverrides::default()
        };

        apply_overrides_to_settings(&mut settings, &overrides);

        assert_eq!(settings.provider.batch_max_output_tokens, Some(12_000));
        assert_eq!(settings.batch.max_items, 2);
        assert_eq!(settings.batch.target_tokens, 4_000);
        assert_eq!(settings.scheduler.concurrency, 3);
        assert_eq!(settings.provider.provider_max_attempts, 5);
        assert!(settings.adaptive_concurrency);
        assert!(!settings.batch.adaptive_sizing);
        assert_eq!(settings.double_check.mode, DoubleCheckMode::Semantic);
    }

    #[test]
    fn legacy_flat_sidecar_loads_as_revision_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overrides.json");
        fs::write(
            &path,
            serde_json::to_string_pretty(&RunConfigOverrides {
                concurrency: Some(3),
                ..RunConfigOverrides::default()
            })
            .unwrap(),
        )
        .unwrap();

        let loaded = load_overrides_from_path(&path)
            .unwrap()
            .expect("legacy sidecar should load");

        assert_eq!(loaded.revision, 0);
        assert_eq!(loaded.updated_at_ms, 0);
        assert_eq!(loaded.overrides.concurrency, Some(3));
    }

    #[test]
    fn revisioned_sidecar_merges_and_atomically_replaces_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overrides.json");

        let first = write_merged_overrides_at_path(
            &path,
            RunConfigOverrides {
                concurrency: Some(2),
                ..RunConfigOverrides::default()
            },
        )
        .unwrap();
        let second = write_merged_overrides_at_path(
            &path,
            RunConfigOverrides {
                batch_max_items: Some(4),
                ..RunConfigOverrides::default()
            },
        )
        .unwrap();

        assert_eq!(first.revision, 1);
        assert_eq!(second.revision, 2);
        assert_eq!(second.overrides.concurrency, Some(2));
        assert_eq!(second.overrides.batch_max_items, Some(4));
        assert_eq!(load_overrides_from_path(&path).unwrap().unwrap(), second);
        assert!(!path.with_extension("lock").exists());
        let leftovers = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".staged-"))
            .count();
        assert_eq!(leftovers, 0, "atomic writer must clean staged siblings");
    }

    #[test]
    fn sidecar_rejects_zero_and_unknown_settings() {
        let zero = RunConfigOverrides {
            concurrency: Some(0),
            ..RunConfigOverrides::default()
        };
        assert!(
            zero.validate()
                .unwrap_err()
                .to_string()
                .contains("concurrency")
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overrides.json");
        fs::write(&path, r#"{"concurrency":2,"surprise":true}"#).unwrap();
        let error = load_overrides_from_path(&path).expect_err("unknown field must reject");
        assert!(error.to_string().contains("failed to parse"));
    }

    #[test]
    fn writer_recovers_an_unreadable_sidecar_with_a_fresh_envelope() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overrides.json");
        fs::write(&path, r#"{"concurrency":0,"surprise":true}"#).unwrap();

        let recovered = write_merged_overrides_at_path(
            &path,
            RunConfigOverrides {
                concurrency: Some(3),
                ..RunConfigOverrides::default()
            },
        )
        .expect("a locked writer should recover a corrupt sidecar");

        assert_eq!(recovered.revision, 1);
        assert_eq!(recovered.overrides.concurrency, Some(3));
        assert_eq!(load_overrides_from_path(&path).unwrap(), Some(recovered));
    }
}
