use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use bookforge_core::{
    GlossaryFormat, ProviderPreset, ResolvedRunSettings,
    config::{ContextScope, DoubleCheckMode, TranslationProfile},
    run_dir_for_job,
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

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(default)]
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

    fn is_empty(&self) -> bool {
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
}

pub async fn run(args: ReconfigureArgs) -> Result<()> {
    reject_immutable_changes(&args)?;
    let incoming = RunConfigOverrides::from_args(&args);
    if incoming.is_empty() {
        anyhow::bail!(
            "no mutable settings provided; reconfigure accepts cache-safe scheduling, budget, QA, double-check, validation, provider-attempt, and adaptive flags"
        );
    }

    let store = JobStore::open_default()?;
    let Some(job) = store.get_job(&args.job_id)? else {
        anyhow::bail!("job '{}' was not found", args.job_id);
    };
    if job.status != "paused" {
        anyhow::bail!(
            "job '{}' is '{}'; reconfigure only applies to paused jobs. Run `bookforge pause {}` first, or start a fresh run for immutable settings.",
            args.job_id,
            job.status,
            args.job_id
        );
    }
    if store.load_job_config_snapshot(&args.job_id)?.is_none() {
        anyhow::bail!(
            "job '{}' does not have a run configuration snapshot; it cannot be reconfigured safely",
            args.job_id
        );
    }

    let existing = load_overrides_for_job(&args.job_id)?;
    let merged = incoming.merge(existing.unwrap_or_default());
    let path = write_overrides_for_job(&args.job_id, &merged)?;
    println!("Reconfigured: {}", args.job_id);
    println!("Overrides: {}", path.display());
    for line in describe_overrides(&merged) {
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
        "Stop the paused run first: `bookforge stop {job_id}`, then `bookforge resume {job_id}` to apply overrides. If the paused process is already gone, use `bookforge resume {job_id} --force`."
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

fn load_overrides_from_path(path: &Path) -> Result<Option<RunConfigOverrides>> {
    match fs::read_to_string(path) {
        Ok(contents) => serde_json::from_str(&contents)
            .map(Some)
            .with_context(|| format!("failed to parse {}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
    }
}

fn write_overrides_for_job(job_id: &str, overrides: &RunConfigOverrides) -> Result<PathBuf> {
    let path = overrides_path_for_job(job_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(overrides)?;
    fs::write(&path, format!("{json}\n"))
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
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
}
