use std::{collections::HashMap, sync::Arc};

use anyhow::Result;
use bookforge_core::{
    ResolvedRunSettings, glossary::GlossarySelectionRule, scheduler::SchedulerConfig,
    segment::Segment,
};
use bookforge_llm::{LlmProvider, SegmentTranslation, TelemetryLog, TranslationRunConfig};
use bookforge_store::JobStore;

use crate::checkpoint::CheckpointWriter;

use super::{
    CheckpointContext, ProgressRequestProvider, checkpointing::finalize_writer,
    translate_and_checkpoint, translate_and_checkpoint_batch,
};

pub(crate) struct CheckpointRunContext<'a> {
    pub store: &'a JobStore,
    pub job_id: &'a str,
    pub provider: &'a str,
    pub model: &'a str,
    pub prompt_version: &'a str,
}

pub(crate) fn batch_run_config(
    run_config: &TranslationRunConfig,
    settings: &ResolvedRunSettings,
) -> TranslationRunConfig {
    TranslationRunConfig {
        source_language: run_config.source_language.clone(),
        target_language: run_config.target_language.clone(),
        provider: run_config.provider.clone(),
        model: run_config.model.clone(),
        prompt_version: run_config.prompt_version.clone(),
        temperature: run_config.temperature,
        scheduler: SchedulerConfig {
            concurrency: run_config.scheduler.concurrency,
            max_attempts: settings.provider.provider_max_attempts,
        },
        profile: settings.profile,
        model_context_tokens: settings.provider.model_context_tokens,
        max_output_tokens: settings.provider.max_output_tokens,
        batch_max_output_tokens: settings.provider.batch_max_output_tokens,
        compact_prompts: settings.compact_prompts,
        glossary: run_config.glossary.clone(),
        context: run_config.context,
        context_registry: run_config.context_registry.clone(),
        style: run_config.style.clone(),
        entities: run_config.entities.clone(),
        pause_signal: run_config.pause_signal.clone(),
        runtime_settings: run_config.runtime_settings.clone(),
    }
}

pub(crate) async fn run_checkpointed_translation<P>(
    provider: P,
    pending_segments: &[Segment],
    run_config: &TranslationRunConfig,
    settings: &ResolvedRunSettings,
    checkpoint: CheckpointRunContext<'_>,
    progress: Arc<dyn bookforge_core::ProgressSink>,
    batch_enabled: bool,
) -> Result<Vec<SegmentTranslation>>
where
    P: LlmProvider,
{
    run_checkpointed_translation_inner(
        provider,
        pending_segments,
        run_config,
        settings,
        checkpoint,
        progress,
        batch_enabled,
        Arc::new(TelemetryLog::new()),
        &HashMap::new(),
        false,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_checkpointed_translation_instrumented<P>(
    provider: P,
    pending_segments: &[Segment],
    run_config: &TranslationRunConfig,
    settings: &ResolvedRunSettings,
    checkpoint: CheckpointRunContext<'_>,
    progress: Arc<dyn bookforge_core::ProgressSink>,
    batch_enabled: bool,
    telemetry: Arc<TelemetryLog>,
    glossary_rules: &HashMap<String, Vec<GlossarySelectionRule>>,
    print_human_output: bool,
) -> Result<Vec<SegmentTranslation>>
where
    P: LlmProvider,
{
    run_checkpointed_translation_inner(
        provider,
        pending_segments,
        run_config,
        settings,
        checkpoint,
        progress,
        batch_enabled,
        telemetry,
        glossary_rules,
        print_human_output,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_checkpointed_translation_inner<P>(
    provider: P,
    pending_segments: &[Segment],
    run_config: &TranslationRunConfig,
    settings: &ResolvedRunSettings,
    checkpoint: CheckpointRunContext<'_>,
    progress: Arc<dyn bookforge_core::ProgressSink>,
    batch_enabled: bool,
    telemetry: Arc<TelemetryLog>,
    glossary_rules: &HashMap<String, Vec<GlossarySelectionRule>>,
    print_human_output: bool,
) -> Result<Vec<SegmentTranslation>>
where
    P: LlmProvider,
{
    if pending_segments.is_empty() {
        return Ok(Vec::new());
    }

    let writer = CheckpointWriter::spawn(checkpoint.store.path().to_path_buf(), progress.clone());
    let sender = writer.sender();
    let pause_signal = run_config.pause_signal.clone().unwrap_or_default();
    let mut controlled_config = run_config.clone();
    controlled_config.pause_signal = Some(pause_signal);
    let mut control_poller = crate::control::ControlFilePoller::new(
        checkpoint.store,
        checkpoint.job_id,
        progress.clone(),
    );
    let checkpoint_context = CheckpointContext {
        store: checkpoint.store,
        job_id: checkpoint.job_id,
        provider: checkpoint.provider,
        model: checkpoint.model,
        prompt_version: checkpoint.prompt_version,
        sender: &sender,
    };
    let translation_result = if batch_enabled {
        let batch_config = batch_run_config(&controlled_config, settings);
        translate_and_checkpoint_batch(
            provider,
            pending_segments,
            &batch_config,
            settings,
            checkpoint_context,
            progress,
            Some(&mut control_poller),
            telemetry.clone(),
            print_human_output,
        )
        .await
    } else {
        translate_and_checkpoint(
            ProgressRequestProvider::new(provider, progress),
            pending_segments,
            &controlled_config,
            checkpoint_context,
            Some(&mut control_poller),
        )
        .await
    };
    let translations = finalize_writer(translation_result, sender, writer).await?;
    record_glossary_telemetry(
        &telemetry,
        &run_config.glossary,
        glossary_rules,
        &translations,
    );
    Ok(translations)
}

pub(crate) fn record_glossary_telemetry(
    telemetry: &TelemetryLog,
    glossary: &bookforge_llm::GlossaryRunConfig,
    rules_by_segment: &HashMap<String, Vec<GlossarySelectionRule>>,
    translations: &[SegmentTranslation],
) {
    for translation in translations {
        let Some(entries) = glossary.entries_by_segment.get(&translation.segment_id.0) else {
            continue;
        };
        let Some(rules) = rules_by_segment.get(&translation.segment_id.0) else {
            continue;
        };
        debug_assert_eq!(entries.len(), rules.len());
        let output = translation.joined_text();
        for (entry, rule) in entries.iter().zip(rules) {
            let honored = if entry.target.is_empty() {
                false
            } else if entry.case_sensitive {
                output.contains(&entry.target)
            } else {
                output.to_lowercase().contains(&entry.target.to_lowercase())
            };
            telemetry.record_glossary_entry(*rule, honored);
        }
    }
}
