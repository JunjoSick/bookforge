use anyhow::Result;
#[cfg(test)]
use bookforge_core::config::TranslationProfile;
use bookforge_core::{
    FallbackRunConfigSnapshot, GlossaryFormat, GlossaryTerm, NullProgressSink, RunConfigSnapshot,
    config::{
        DoubleCheckMode, FallbackScope, PromptVersion, ResolvedRunSettings, TranslationConfig,
    },
    marker::{parse_empty_marker, parse_marker_close, parse_paired_marker_open},
    merge_scope_terms,
    scheduler::SchedulerConfig,
    segment::{Segment, SegmentStatus, build_segments, compute_cache_namespace},
    select_glossary_for_segments,
};
use bookforge_epub::read_epub;
#[cfg(test)]
use bookforge_llm::translate_segments;
use bookforge_llm::{
    AdaptiveLimiter, BatchMode, CompletionRequest, CompletionResponse, ContextRegistry,
    ContextRunConfig, EntityRunConfig, GlossaryRunConfig, LlmError, LlmProvider, MockMode,
    MockProvider, OpenAiCompatibleConfig, OpenAiCompatibleProvider, ProviderCapabilities,
    ProviderRateController, QaSegmentReview, RateControllerConfig, SegmentTranslation,
    StyleRunConfig, TelemetryLog, TranslationRunConfig, account_for_batch_prompt_overhead,
    build_translation_batches, qa_segments_parallel_with_max_output_tokens, run_double_check,
    telemetry_summary, translate_batches_with_callback, translate_batches_with_control,
    translate_segments_with_callback, translate_segments_with_control,
};
use bookforge_store::{CreateJob, JobRecord, JobStore, SaveTranslation};
use clap::Args;
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

#[cfg(test)]
use crate::LanguageArgs;
use crate::{
    ProviderArgs as CliProviderArgs, QaMode,
    checkpoint::{CheckpointCommand, CheckpointSender, CheckpointWriter},
    commands::{glossary::read_glossary_file, reconfigure},
    default_output_path,
};

pub mod args;
mod cache;
mod checkpointing;
mod engine;
mod finalization;
mod orchestration;
mod reporting;
mod settings;
mod snapshot;

pub use args::TranslateArgs;
pub(crate) use cache::{CacheContext, apply_cached_translations, pending_segments_for_job};
use checkpointing::finalize_writer;
pub(crate) use engine::{CheckpointRunContext, run_checkpointed_translation};
use engine::{record_glossary_telemetry, run_checkpointed_translation_instrumented};
#[cfg(test)]
use finalization::suspicious_qa_candidates;
pub(crate) use finalization::{
    apply_double_check_corrections, job_was_stopped, mark_job_finished,
    persist_corrected_translations, print_stopped_resume_hint, qa_reviews_for_mode,
};
use finalization::{finish_translation_pipeline, mark_unfinished_segments_failed};
use orchestration::human_stdout_enabled;
pub use orchestration::run;
use reporting::print_summary_rebuild_and_report;
pub(crate) use reporting::{rebuild_options_from_snapshot, regenerate_report_after_correction};
use settings::{
    apply_plan_recommendations, apply_provider_preset, resolve_settings,
    retry_amplification_warning,
};
use snapshot::persist_snapshot;

#[derive(Debug, Clone)]
pub(crate) struct FallbackPassConfig {
    provider: String,
    model: String,
    base_url: Option<String>,
    api_key_env: Option<String>,
    scope: FallbackScope,
}

impl FallbackPassConfig {
    pub(crate) fn from_snapshot(snapshot: Option<&FallbackRunConfigSnapshot>) -> Option<Self> {
        snapshot.map(|fallback| Self {
            provider: fallback.provider.clone(),
            model: fallback.model.clone(),
            base_url: fallback.base_url.clone(),
            api_key_env: fallback.api_key_env.clone(),
            scope: fallback.scope,
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedGlossary {
    pub run_config: GlossaryRunConfig,
    pub fingerprint: String,
    pub active_terms: Vec<GlossaryTerm>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn context_run_config_from_args(cli_args: &TranslateArgs) -> ContextRunConfig {
    ContextRunConfig {
        window: cli_args.context_window,
        budget_tokens: cli_args.context_budget_tokens,
        scope: cli_args.context_scope,
        strict: cli_args.context_strict,
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedStyle {
    pub run_config: Option<StyleRunConfig>,
    pub fingerprint: String,
    pub rendered_block: String,
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedEntities {
    pub run_config: Option<EntityRunConfig>,
    pub fingerprint: String,
    pub rendered_block: String,
}

/// Import `--entities` files into the store, then load all entities
/// applicable to the current (source_language, target_language, book_id,
/// series_id) scope and produce both the runtime injection bundle and
/// the snapshot-capture fields.
pub(crate) fn prepare_entities_run_config(
    store: &JobStore,
    entity_files: &[PathBuf],
    source_language: Option<&str>,
    target_language: &str,
    book_id: Option<&str>,
    series_id: Option<&str>,
) -> Result<PreparedEntities> {
    for path in entity_files {
        let entities = crate::commands::entity::read_entities_file(path)?;
        crate::commands::entity::upsert_entities(store, &entities)?;
    }
    // Without a source language we can't load entities (they're keyed on
    // both languages). The run still needs a stable fingerprint.
    let Some(source_language) = source_language else {
        let fp = bookforge_core::entity::entities_fingerprint(&[]);
        return Ok(PreparedEntities {
            run_config: None,
            fingerprint: fp,
            rendered_block: String::new(),
        });
    };
    let stored =
        store.load_active_entities(source_language, target_language, book_id, series_id)?;
    let active: Vec<bookforge_core::entity::Entity> = stored
        .into_iter()
        .map(|r| bookforge_core::entity::Entity {
            id: Some(r.id),
            scope_kind: r.scope_kind,
            scope_id: r.scope_id,
            source_name: r.source_name,
            target_name: r.target_name,
            gender_target: r.gender_target,
            role: r.role,
            notes: r.notes,
            source_language: r.source_language,
            target_language: r.target_language,
        })
        .collect();
    let merged = bookforge_core::entity::merge_scope_entities(&active);
    let rendered_block = bookforge_core::entity::render_entity_agreement_block(&merged);
    let fingerprint = bookforge_core::entity::entities_fingerprint(&merged);
    let run_config = if rendered_block.is_empty() {
        None
    } else {
        Some(EntityRunConfig {
            rendered_block: rendered_block.clone(),
            fingerprint: fingerprint.clone(),
        })
    };
    Ok(PreparedEntities {
        run_config,
        fingerprint,
        rendered_block,
    })
}

/// Load `--style` files into the store, merge all sheets for the active
/// scope, render the prompt block, and produce both the run-time
/// injection bundle and the snapshot-capture fields.
pub(crate) fn prepare_style_run_config(
    store: &JobStore,
    style_files: &[PathBuf],
    target_language: &str,
    book_id: Option<&str>,
    series_id: Option<&str>,
) -> Result<PreparedStyle> {
    for path in style_files {
        let sheet = crate::commands::style::read_style_file(path)?;
        let content_toml = std::fs::read_to_string(path)?;
        let one_sheet = vec![sheet.clone()];
        let merged_for_one = bookforge_core::style::merge_style_sheets(&one_sheet);
        let fp = bookforge_core::style::style_fingerprint(merged_for_one.as_ref());
        store.upsert_style_sheet(&bookforge_store::NewStyleSheet {
            scope_kind: sheet.scope_kind,
            scope_id: sheet.scope_id.as_deref(),
            target_language: &sheet.target_language,
            content_toml: &content_toml,
            fingerprint: &fp,
        })?;
    }
    let stored = store.load_active_style_sheets(target_language, book_id, series_id)?;
    // Target-specific built-ins establish the minimum viable translation
    // contract. User sheets are appended afterwards, so equal-scope scalar
    // values from an explicit sheet win during the stable merge.
    let mut parsed: Vec<bookforge_core::style::StyleSheet> =
        bookforge_core::style::built_in_style_for_target(target_language)
            .into_iter()
            .collect();
    for record in &stored {
        match crate::commands::style::parse_style_toml(&record.content_toml) {
            Ok(sheet) => parsed.push(sheet),
            Err(err) => tracing::warn!(
                style_id = record.id,
                "skipping stored style sheet that failed to parse: {err}"
            ),
        }
    }
    let merged = bookforge_core::style::merge_style_sheets(&parsed);
    let rendered_block = bookforge_core::style::render_style_block(merged.as_ref());
    let fingerprint = bookforge_core::style::style_fingerprint(merged.as_ref());
    let run_config = if rendered_block.is_empty() {
        None
    } else {
        Some(StyleRunConfig {
            rendered_block: rendered_block.clone(),
            fingerprint: fingerprint.clone(),
        })
    };
    Ok(PreparedStyle {
        run_config,
        fingerprint,
        rendered_block,
    })
}

/// Seed the in-memory completion fence from already-completed translations
/// (cache hits, prior runs). Without this the fence would deadlock on
/// segments that won't be re-translated this run.
pub(crate) fn prepopulate_context_registry(
    registry: Option<&Arc<ContextRegistry>>,
    segments: &[Segment],
    translations: &[SegmentTranslation],
) {
    let Some(registry) = registry else {
        return;
    };
    let by_id: std::collections::HashMap<_, _> = segments.iter().map(|s| (&s.id, s)).collect();
    for translation in translations {
        if let Some(segment) = by_id.get(&translation.segment_id) {
            registry.pre_populate(segment, translation);
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_glossary_run_config(
    store: &JobStore,
    glossary_files: &[PathBuf],
    source_language: Option<&str>,
    target_language: &str,
    book_id: Option<&str>,
    series_id: Option<&str>,
    format: GlossaryFormat,
    budget_tokens: usize,
    prompt_extra: Option<String>,
    segments: &[Segment],
) -> Result<PreparedGlossary> {
    let imported_terms = import_glossary_files(store, glossary_files)?;
    let mut active_terms = if let Some(source_language) = source_language {
        store.load_active_glossary_terms(source_language, target_language, book_id, series_id)?
    } else {
        store.load_active_glossary_terms_for_target(target_language, book_id, series_id)?
    };
    active_terms.extend(imported_terms.into_iter().filter(|term| {
        term.active()
            && term.target_language == target_language
            && source_language.is_none_or(|source| term.source_language == source)
    }));
    active_terms = merge_scope_terms(&active_terms);
    let selected = select_glossary_for_segments(segments, &active_terms, budget_tokens);
    if selected.truncated_authoritative_entries > 0 {
        tracing::warn!(
            count = selected.truncated_authoritative_entries,
            "glossary token budget dropped authoritative entries"
        );
    }
    let fingerprint = glossary_fingerprint(
        format,
        budget_tokens,
        prompt_extra.as_deref(),
        &active_terms,
    );
    Ok(PreparedGlossary {
        run_config: GlossaryRunConfig {
            format,
            entries_by_segment: selected.entries_by_segment,
            prompt_extra,
            guidance_by_segment: std::collections::HashMap::new(),
        },
        fingerprint,
        active_terms,
    })
}

fn import_glossary_files(
    store: &JobStore,
    glossary_files: &[PathBuf],
) -> Result<Vec<GlossaryTerm>> {
    let mut imported = Vec::new();
    for path in glossary_files {
        let terms = read_glossary_file(path)?;
        store.upsert_glossary_terms(&terms)?;
        imported.extend(terms);
    }
    Ok(imported)
}

pub(crate) fn glossary_fingerprint(
    format: GlossaryFormat,
    budget_tokens: usize,
    prompt_extra: Option<&str>,
    terms: &[GlossaryTerm],
) -> String {
    let mut normalized = terms.to_vec();
    for term in &mut normalized {
        term.id = None;
    }
    normalized.sort_by(|a, b| {
        a.source_language
            .cmp(&b.source_language)
            .then_with(|| a.target_language.cmp(&b.target_language))
            .then_with(|| a.scope_kind.priority().cmp(&b.scope_kind.priority()))
            .then_with(|| a.scope_id.cmp(&b.scope_id))
            .then_with(|| a.source_text.cmp(&b.source_text))
            .then_with(|| a.target_text.cmp(&b.target_text))
    });
    let payload = serde_json::json!({
        "schema": 1,
        "format": format.as_str(),
        "budget_tokens": budget_tokens,
        "prompt_extra": prompt_extra.unwrap_or(""),
        "terms": normalized,
    });
    let serialized = serde_json::to_vec(&payload).unwrap_or_default();
    let digest = Sha256::digest(serialized);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut hex, "{byte:02x}").expect("writing to string cannot fail");
    }
    hex
}

pub(crate) fn mock_mode(model: &str) -> MockMode {
    match model {
        "mock-identity" => MockMode::Identity,
        "mock-uppercase" => MockMode::Uppercase,
        "mock-malformed-json" => MockMode::MalformedJson,
        "mock-wrong-segment-id" => MockMode::WrongSegmentId,
        _ => MockMode::PrefixTarget,
    }
}

#[derive(Clone)]
pub(crate) struct ProgressRequestProvider<P> {
    inner: P,
    progress: Arc<dyn bookforge_core::ProgressSink>,
    counter: Arc<AtomicUsize>,
    request_prefix: Option<&'static str>,
}

impl<P> ProgressRequestProvider<P> {
    pub(crate) fn new(inner: P, progress: Arc<dyn bookforge_core::ProgressSink>) -> Self {
        Self::new_with_prefix(inner, progress, None)
    }

    pub(crate) fn new_with_prefix(
        inner: P,
        progress: Arc<dyn bookforge_core::ProgressSink>,
        request_prefix: Option<&'static str>,
    ) -> Self {
        Self {
            inner,
            progress,
            counter: Arc::new(AtomicUsize::new(0)),
            request_prefix,
        }
    }
}

impl<P> LlmProvider for ProgressRequestProvider<P>
where
    P: LlmProvider,
{
    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> std::result::Result<CompletionResponse, LlmError> {
        let metadata = request.metadata.clone();
        let max_output_tokens = request.max_output_tokens;
        let prefix = self
            .request_prefix
            .unwrap_or_else(|| finalize_request_prefix(metadata.prompt_template.as_deref()));
        let request_id = format!(
            "{prefix}_{:04}",
            self.counter.fetch_add(1, Ordering::Relaxed)
        );
        self.progress
            .emit(bookforge_core::ProgressEvent::RequestStarted {
                request_id: request_id.clone(),
                batch_id: None,
                segment_id: metadata.segment_id.clone(),
                provider: metadata.provider.clone(),
                model: metadata.model.clone(),
                prompt_template: metadata.prompt_template.clone(),
                items: metadata.block_ids.len().max(1),
                estimated_input_tokens: 0,
                max_output_tokens,
                active_requests: 1,
                target_concurrency: 1,
                runtime_config_revision: metadata.runtime_config_revision,
                provider_max_attempts: metadata.provider_max_attempts,
                timestamp_ms: bookforge_core::progress::now_ms(),
            });

        let started = Instant::now();
        let result = self.inner.complete(request).await;
        let (status, finish_reason, input_tokens, output_tokens, error_kind) = match &result {
            Ok(response) => (
                "ok".to_string(),
                Some(format!("{:?}", response.finish_reason)),
                response.input_tokens,
                response.output_tokens,
                None,
            ),
            Err(error) => (
                "error".to_string(),
                None,
                None,
                None,
                Some(classify_error(error).to_string()),
            ),
        };
        self.progress
            .emit(bookforge_core::ProgressEvent::RequestFinished {
                request_id,
                batch_id: None,
                segment_id: metadata.segment_id,
                status,
                latency_ms: started.elapsed().as_millis() as u64,
                status_code: None,
                finish_reason,
                retry_count: 0,
                input_tokens,
                output_tokens,
                error_kind,
                timestamp_ms: bookforge_core::progress::now_ms(),
            });
        result
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.inner.capabilities()
    }

    fn is_reasoning(&self) -> bool {
        self.inner.is_reasoning()
    }
}

fn finalize_request_prefix(prompt_template: Option<&str>) -> &'static str {
    match prompt_template {
        Some("qa_batch" | "qa_segment") => "qa",
        Some("correct_batch") => "repair",
        Some("double_check_batch") => "double_check",
        _ => "finalize",
    }
}

#[allow(clippy::too_many_arguments)]
fn provider_config(
    provider: &str,
    model: Option<&str>,
    base_url: Option<&str>,
    api_key_env: Option<&str>,
    timeout_seconds: u64,
    provider_max_attempts: usize,
    thinking_disabled: bool,
    retry_after_policy: bookforge_core::RetryAfterPolicy,
    max_backoff_seconds: u64,
    max_idle_per_host: usize,
    json_mode: bookforge_core::JsonMode,
) -> Result<OpenAiCompatibleConfig> {
    let (default_url, default_key_env, default_model) = match provider {
        "deepseek" => (
            "https://api.deepseek.com/v1",
            "DEEPSEEK_API_KEY",
            "deepseek-v4-flash",
        ),
        "openrouter" => (
            "https://openrouter.ai/api/v1",
            "OPENROUTER_API_KEY",
            "openrouter/auto",
        ),
        "openai-compatible" => (
            base_url.ok_or_else(|| {
                anyhow::anyhow!("--base-url is required for --provider openai-compatible")
            })?,
            "OPENAI_API_KEY",
            model.ok_or_else(|| {
                anyhow::anyhow!("--model is required for --provider openai-compatible")
            })?,
        ),
        _ => {
            return Err(anyhow::anyhow!(
                "unsupported translation provider '{provider}'"
            ));
        }
    };

    Ok(OpenAiCompatibleConfig {
        base_url: base_url
            .map(String::from)
            .unwrap_or_else(|| default_url.to_string()),
        api_key_env: api_key_env
            .map(String::from)
            .unwrap_or_else(|| default_key_env.to_string()),
        model: model
            .or(Some(default_model))
            .map(String::from)
            .unwrap_or_else(|| default_model.to_string()),
        timeout_seconds,
        provider_max_attempts: provider_max_attempts.max(1),
        thinking_disabled,
        retry_after_policy,
        max_backoff_seconds,
        max_idle_per_host,
        json_mode,
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn translate_and_checkpoint_batch<P>(
    provider: P,
    segments: &[Segment],
    config: &TranslationRunConfig,
    settings: &ResolvedRunSettings,
    checkpoint: CheckpointContext<'_>,
    progress: Arc<dyn bookforge_core::ProgressSink>,
    mut control: Option<&mut crate::control::ControlFilePoller<'_>>,
    telemetry: Arc<TelemetryLog>,
    print_human_output: bool,
) -> Result<Vec<SegmentTranslation>>
where
    P: LlmProvider,
{
    let mut batches = build_translation_batches(segments, &settings.batch, settings.profile);
    apply_text_only_retry_guidance(&mut batches, config);
    let batches = account_for_batch_prompt_overhead(batches, &settings.batch, config);

    if batches.is_empty() {
        return translate_and_checkpoint(provider, segments, config, checkpoint, control).await;
    }

    if print_human_output {
        eprintln!("Batches: {}", batches.len());
    }

    // Keep the adaptive controller alive even while disabled so a live
    // revision can enable it at the next request boundary without rebuilding
    // or losing its limiter state. The batch engine bypasses it while the
    // current runtime snapshot has adaptive concurrency disabled.
    let rate_controller = {
        let limiter = Arc::new(AdaptiveLimiter::new_with_bounds(
            settings.scheduler.concurrency.max(1),
            1,
            (settings.scheduler.concurrency * 4).max(1),
            std::time::Duration::from_secs(2),
            Some(progress.clone()),
        ));
        Some(Arc::new(ProviderRateController::new(
            limiter,
            RateControllerConfig::for_target(settings.scheduler.concurrency.max(1)),
        )))
    };

    let mut batch_sizer = settings.batch.adaptive_sizing.then(|| {
        bookforge_llm::BatchSizer::with_progress(
            settings.batch.target_tokens,
            settings.batch.max_items,
            progress.clone(),
        )
    });

    let sender = checkpoint.sender.clone();
    let (finalized_tx, mut finalized_rx) = tokio::sync::mpsc::channel::<SegmentTranslation>(64);

    let checkpoint_handle = {
        let sender = sender.clone();
        let job_id = checkpoint.job_id.to_string();
        let provider_name = checkpoint.provider.to_string();
        let model = checkpoint.model.to_string();
        let prompt_version = checkpoint.prompt_version.to_string();
        tokio::spawn(async move {
            while let Some(translation) = finalized_rx.recv().await {
                sender
                    .send(CheckpointCommand::SaveTranslation {
                        job_id: job_id.clone(),
                        translation: Box::new(translation),
                        provider: provider_name.clone(),
                        model: model.clone(),
                        prompt_version: prompt_version.clone(),
                    })
                    .await
                    .map_err(|e| {
                        bookforge_llm::LlmError::Provider(format!("checkpoint send failed: {e}"))
                    })?;
            }
            Ok::<(), bookforge_llm::LlmError>(())
        })
    };

    let batch_result = match control.as_mut() {
        Some(control) => {
            translate_batches_with_control(
                provider,
                batches,
                segments,
                config,
                telemetry.clone(),
                rate_controller,
                batch_sizer.as_mut(),
                progress.clone(),
                Some(finalized_tx),
                |_| Ok(()),
                |signal| {
                    control
                        .poll(signal)
                        .map_err(|err| bookforge_llm::LlmError::Provider(err.to_string()))
                },
            )
            .await
        }
        None => {
            translate_batches_with_callback(
                provider,
                batches,
                segments,
                config,
                telemetry.clone(),
                rate_controller,
                batch_sizer.as_mut(),
                progress.clone(),
                Some(finalized_tx),
                |_| Ok(()),
            )
            .await
        }
    };

    let checkpoint_result = checkpoint_handle.await;

    match (batch_result, checkpoint_result) {
        (Ok(translations), Ok(Ok(()))) => {
            let snapshot = telemetry.snapshot();
            if !snapshot.is_empty() {
                eprintln!("\n{}", telemetry_summary(&snapshot));
            }
            Ok(translations)
        }
        (Err(_), _)
            if config
                .pause_signal
                .as_ref()
                .is_some_and(|signal| signal.is_stopped()) =>
        {
            Ok(Vec::new())
        }
        (Ok(_), Ok(Err(e))) | (Err(e @ bookforge_llm::LlmError::Provider(_)), _) => {
            let message = format!("batch translation checkpoint failure: {e}");
            mark_unfinished_segments_failed(
                checkpoint.store,
                checkpoint.job_id,
                segments,
                &message,
            )?;
            Err(anyhow::anyhow!(message))
        }
        (_, Err(join_err)) => {
            let message = format!("batch checkpoint task panicked: {join_err}");
            mark_unfinished_segments_failed(
                checkpoint.store,
                checkpoint.job_id,
                segments,
                &message,
            )?;
            Err(anyhow::anyhow!(message))
        }
        (Err(error), _) => {
            let message = format!("batch translation failed: {error}");
            mark_unfinished_segments_failed(
                checkpoint.store,
                checkpoint.job_id,
                segments,
                &message,
            )?;
            Err(anyhow::anyhow!(message))
        }
    }
}

fn apply_text_only_retry_guidance(
    batches: &mut [bookforge_llm::TranslationBatch],
    config: &TranslationRunConfig,
) {
    if !config
        .target_language
        .trim()
        .eq_ignore_ascii_case("Toki Pona")
    {
        return;
    }
    for batch in batches {
        let use_text_only = !batch.items.is_empty()
            && batch.items.iter().all(|item| {
                config
                    .glossary
                    .guidance_by_segment
                    .get(&item.segment_id.0)
                    .is_some_and(|guidance| guidance.contains("[bookforge:text-only]"))
            });
        if !use_text_only {
            continue;
        }
        batch.mode = BatchMode::TurboTextOnly;
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_fallback_pass(
    cancel_token: &tokio_util::sync::CancellationToken,
    fallback_config: Option<&FallbackPassConfig>,
    segments: &[Segment],
    translations: Vec<SegmentTranslation>,
    store: &JobStore,
    job_id: &str,
    prompt_version: &str,
    settings: &ResolvedRunSettings,
    primary_run_config: &TranslationRunConfig,
    control: Option<&mut crate::control::ControlFilePoller<'_>>,
    progress: Arc<dyn bookforge_core::ProgressSink>,
) -> Result<Vec<SegmentTranslation>> {
    let telemetry = TelemetryLog::new();
    let glossary_rules = std::collections::HashMap::new();
    run_fallback_pass_instrumented(
        cancel_token,
        fallback_config,
        segments,
        translations,
        store,
        job_id,
        prompt_version,
        settings,
        primary_run_config,
        control,
        progress,
        &telemetry,
        &glossary_rules,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_fallback_pass_instrumented(
    cancel_token: &tokio_util::sync::CancellationToken,
    fallback_config: Option<&FallbackPassConfig>,
    segments: &[Segment],
    mut translations: Vec<SegmentTranslation>,
    store: &JobStore,
    job_id: &str,
    prompt_version: &str,
    settings: &ResolvedRunSettings,
    primary_run_config: &TranslationRunConfig,
    control: Option<&mut crate::control::ControlFilePoller<'_>>,
    progress: Arc<dyn bookforge_core::ProgressSink>,
    telemetry: &TelemetryLog,
    glossary_rules: &std::collections::HashMap<
        String,
        Vec<bookforge_core::glossary::GlossarySelectionRule>,
    >,
) -> Result<Vec<SegmentTranslation>> {
    let Some(fallback_config) = fallback_config else {
        return Ok(translations);
    };
    let provider_str = fallback_config.provider.as_str();
    let model_str = fallback_config.model.as_str();
    let fallback_scope = fallback_config.scope;
    let fallback_base_url = fallback_config.base_url.as_deref();
    let fallback_api_key_env = fallback_config.api_key_env.as_deref();

    let fallback_status_by_segment = store
        .segment_records(job_id)?
        .into_iter()
        .map(|record| (record.id, record.status))
        .collect::<std::collections::HashMap<_, _>>();
    let candidates: Vec<Segment> = segments
        .iter()
        .filter(|s| {
            let t = translations.iter().find(|t| t.segment_id.0 == s.id.0);
            match t {
                Some(t) => match fallback_scope {
                    FallbackScope::Failed => t.status == SegmentStatus::Failed,
                    FallbackScope::NeedsReview => t.status == SegmentStatus::NeedsReview,
                    FallbackScope::FailedAndNeedsReview => {
                        t.status == SegmentStatus::Failed || t.status == SegmentStatus::NeedsReview
                    }
                },
                None => {
                    let Some(status) = fallback_status_by_segment.get(&s.id.0) else {
                        return false;
                    };
                    match fallback_scope {
                        FallbackScope::Failed => status == "failed",
                        FallbackScope::NeedsReview => status == "needs_review",
                        FallbackScope::FailedAndNeedsReview => {
                            status == "failed" || status == "needs_review"
                        }
                    }
                }
            }
        })
        .cloned()
        .collect();

    if candidates.is_empty() {
        return Ok(translations);
    }

    println!(
        "Fallback: retrying {} segments with {}/{}",
        candidates.len(),
        provider_str,
        model_str
    );

    let run_config = TranslationRunConfig {
        source_language: primary_run_config.source_language.clone(),
        target_language: primary_run_config.target_language.clone(),
        provider: provider_str.to_string(),
        model: model_str.to_string(),
        prompt_version: prompt_version.to_string(),
        // Recovery favors reproducibility and low provider pressure: mirror
        // the primary pass temperature, but dispatch one segment at a time.
        temperature: 0.2,
        scheduler: SchedulerConfig {
            concurrency: 1,
            max_attempts: settings.provider.provider_max_attempts,
        },
        profile: settings.profile,
        model_context_tokens: settings.provider.model_context_tokens,
        max_output_tokens: settings.provider.max_output_tokens,
        batch_max_output_tokens: settings.provider.batch_max_output_tokens,
        compact_prompts: settings.compact_prompts,
        glossary: primary_run_config.glossary.clone(),
        context: primary_run_config.context,
        context_registry: primary_run_config.context_registry.clone(),
        style: primary_run_config.style.clone(),
        entities: primary_run_config.entities.clone(),
        pause_signal: Some(primary_run_config.pause_signal.clone().unwrap_or_default()),
        runtime_settings: primary_run_config.runtime_settings.clone(),
    }; // fallback_run_config

    let writer = CheckpointWriter::spawn(store.path().to_path_buf(), Arc::new(NullProgressSink));
    let sender = writer.sender();
    let checkpoint = CheckpointContext {
        store,
        job_id,
        provider: provider_str,
        model: model_str,
        prompt_version,
        sender: &sender,
    };

    let translation_result = match provider_str {
        "mock" => {
            let fallback = MockProvider::new(mock_mode(model_str), &run_config.target_language);
            translate_and_checkpoint(
                ProgressRequestProvider::new_with_prefix(fallback, progress, Some("fallback")),
                &candidates,
                &run_config,
                checkpoint,
                control,
            )
            .await
        }
        "deepseek" | "openrouter" | "openai-compatible" => {
            let provider_config = provider_config(
                provider_str,
                Some(model_str),
                fallback_base_url,
                fallback_api_key_env,
                settings.provider.timeout_seconds,
                settings.provider.provider_max_attempts,
                settings.provider.thinking_disabled,
                settings.provider.retry_after_policy,
                settings.provider.max_backoff_seconds,
                settings.provider.max_idle_per_host,
                settings.provider.json_mode,
            )?;
            let fallback =
                OpenAiCompatibleProvider::new_with_cancel(provider_config, cancel_token.clone())
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
            translate_and_checkpoint(
                ProgressRequestProvider::new_with_prefix(fallback, progress, Some("fallback")),
                &candidates,
                &run_config,
                checkpoint,
                control,
            )
            .await
        }
        provider => anyhow::bail!("unsupported fallback provider '{provider}'"),
    };
    let fresh = finalize_writer(translation_result, sender, writer).await?;
    record_glossary_telemetry(telemetry, &run_config.glossary, glossary_rules, &fresh);

    for ft in &fresh {
        if let Some(existing) = translations
            .iter_mut()
            .find(|t| t.segment_id.0 == ft.segment_id.0)
        {
            *existing = ft.clone();
        } else {
            translations.push(ft.clone());
        }
    }
    translations.sort_by_key(|translation| translation.ordinal);

    Ok(translations)
}

#[cfg(test)]
pub(crate) async fn translate_with_scheduler_guard<P>(
    provider: P,
    store: &JobStore,
    job_id: &str,
    segments: &[Segment],
    config: &TranslationRunConfig,
) -> Result<Vec<SegmentTranslation>>
where
    P: LlmProvider,
{
    match translate_segments(provider, segments, config).await {
        Ok(translations) => Ok(translations),
        Err(error) => {
            let message = format!(
                "translation scheduler failed before producing per-segment results: {error}"
            );
            mark_unfinished_segments_failed(store, job_id, segments, &message)?;
            Err(anyhow::anyhow!(message))
        }
    }
}

#[derive(Clone)]
pub(crate) struct CheckpointContext<'a> {
    pub store: &'a JobStore,
    pub job_id: &'a str,
    pub provider: &'a str,
    pub model: &'a str,
    pub prompt_version: &'a str,
    /// Sender to the SQLite writer actor. Per-segment checkpoint writes
    /// go through this channel so the async hot path never blocks on disk.
    pub sender: &'a CheckpointSender,
}

pub(crate) async fn translate_and_checkpoint<P>(
    provider: P,
    segments: &[Segment],
    config: &TranslationRunConfig,
    checkpoint: CheckpointContext<'_>,
    mut control: Option<&mut crate::control::ControlFilePoller<'_>>,
) -> Result<Vec<SegmentTranslation>>
where
    P: LlmProvider,
{
    let sender = checkpoint.sender.clone();
    let (finalized_tx, mut finalized_rx) = tokio::sync::mpsc::channel::<SegmentTranslation>(64);

    // Spawn a task that checkpoints each finalized translation as it arrives.
    let checkpoint_handle = {
        let sender = sender.clone();
        let job_id = checkpoint.job_id.to_string();
        let provider_name = checkpoint.provider.to_string();
        let model = checkpoint.model.to_string();
        let prompt_version = checkpoint.prompt_version.to_string();
        tokio::spawn(async move {
            while let Some(translation) = finalized_rx.recv().await {
                sender
                    .send(CheckpointCommand::SaveTranslation {
                        job_id: job_id.clone(),
                        translation: Box::new(translation),
                        provider: provider_name.clone(),
                        model: model.clone(),
                        prompt_version: prompt_version.clone(),
                    })
                    .await
                    .map_err(|e| {
                        bookforge_llm::LlmError::Provider(format!("checkpoint send failed: {e}"))
                    })?;
            }
            Ok::<(), bookforge_llm::LlmError>(())
        })
    };

    let translations = match control.as_mut() {
        Some(control) => {
            translate_segments_with_control(
                provider,
                segments,
                config,
                |_| Ok(()),
                Some(finalized_tx),
                |signal| {
                    control
                        .poll(signal)
                        .map_err(|err| bookforge_llm::LlmError::Provider(err.to_string()))
                },
            )
            .await
        }
        None => {
            translate_segments_with_callback(
                provider,
                segments,
                config,
                |_| Ok(()),
                Some(finalized_tx),
            )
            .await
        }
    };

    // Drop finalized_tx so the checkpoint task exits
    // (finalized_tx was moved into translate_segments_with_callback)
    let checkpoint_result = checkpoint_handle.await;

    match (translations, checkpoint_result) {
        (Ok(translations), Ok(Ok(()))) => Ok(translations),
        (Err(_), _)
            if config
                .pause_signal
                .as_ref()
                .is_some_and(|signal| signal.is_stopped()) =>
        {
            Ok(Vec::new())
        }
        (Ok(_), Ok(Err(e))) | (Err(e @ bookforge_llm::LlmError::Provider(_)), _) => {
            let message = format!("translation checkpoint failure: {e}");
            mark_unfinished_segments_failed(
                checkpoint.store,
                checkpoint.job_id,
                segments,
                &message,
            )?;
            Err(anyhow::anyhow!(message))
        }
        (_, Err(join_err)) => {
            let message = format!("checkpoint task panicked: {join_err}");
            mark_unfinished_segments_failed(
                checkpoint.store,
                checkpoint.job_id,
                segments,
                &message,
            )?;
            Err(anyhow::anyhow!(message))
        }
        (Err(error), _) => {
            let message = format!(
                "translation scheduler failed before producing per-segment results: {error}"
            );
            mark_unfinished_segments_failed(
                checkpoint.store,
                checkpoint.job_id,
                segments,
                &message,
            )?;
            Err(anyhow::anyhow!(message))
        }
    }
}

#[derive(Debug, Args)]
pub struct BenchmarkArgs {
    #[command(flatten)]
    pub provider: CliProviderArgs,

    #[arg(long, default_value_t = 5)]
    pub samples: usize,

    #[arg(long, default_value_t = 1000)]
    pub tokens: usize,

    #[arg(long, default_value_t = 1)]
    pub concurrency: usize,
}

pub async fn run_benchmark(args: BenchmarkArgs) -> Result<()> {
    let pigeon = "Sunt piger, et volare nequeunt. Sed cum cibus apparet, mirabiliter currunt.";
    let provider_config = OpenAiCompatibleConfig {
        base_url: args
            .provider
            .base_url
            .clone()
            .unwrap_or_else(|| "https://openrouter.ai/api/v1".to_string()),
        api_key_env: args
            .provider
            .api_key_env
            .clone()
            .unwrap_or_else(|| "OPENROUTER_API_KEY".to_string()),
        model: args
            .provider
            .model
            .clone()
            .unwrap_or_else(|| "openrouter/auto".to_string()),
        timeout_seconds: args.provider.timeout_seconds.unwrap_or(120),
        provider_max_attempts: 6,
        thinking_disabled: false,
        retry_after_policy: bookforge_core::RetryAfterPolicy::JitteredExponential,
        max_backoff_seconds: 30,
        max_idle_per_host: 32,
        json_mode: bookforge_core::JsonMode::Auto,
    }; // benchmark

    let provider = OpenAiCompatibleProvider::new(provider_config.clone())?;
    let model = provider.model().to_string();

    println!("Benchmarking {} / {}", provider_config.base_url, model);
    println!(
        "Samples: {}, Tokens: {}, Concurrency: {}",
        args.samples, args.tokens, args.concurrency
    );
    println!();

    let mut latencies = Vec::with_capacity(args.samples);
    let mut success_count = 0usize;
    let mut failure_count = 0usize;
    let mut ratelimit_count = 0usize;
    let mut timeout_count = 0usize;
    let mut total_output_tokens = 0u64;
    let mut _total_input_tokens = 0u64;

    for i in 0..args.samples {
        let request = bookforge_llm::CompletionRequest {
            system: "You are a translator. Return JSON only: {\"translation\":\"...\"}".to_string(),
            user: format!("Translate: {{\"text\":\"{}\"}} Return JSON.", pigeon),
            response_format: bookforge_llm::ResponseFormat::Json,
            temperature: 0.2,
            max_output_tokens: Some(args.tokens as u32),
            metadata: Default::default(),
        };

        print!("  [{}/{}] ", i + 1, args.samples);
        match provider.complete(request).await {
            Ok(resp) => {
                latencies.push(resp.provider_latency_ms);
                success_count += 1;
                total_output_tokens += resp.output_tokens.unwrap_or(0);
                _total_input_tokens += resp.input_tokens.unwrap_or(0);
                let tok_sec = if resp.provider_latency_ms > 0 {
                    resp.output_tokens.unwrap_or(0) as f64
                        / (resp.provider_latency_ms as f64 / 1000.0)
                } else {
                    0.0
                };
                println!(
                    "OK {}ms finish={:?} in={:?} out={:?} ~{tok_sec:.0}tok/s",
                    resp.provider_latency_ms,
                    resp.finish_reason,
                    resp.input_tokens,
                    resp.output_tokens
                );
            }
            Err(e) => {
                failure_count += 1;
                let kind = classify_error(&e);
                match kind {
                    "rate_limit" => ratelimit_count += 1,
                    "timeout" => timeout_count += 1,
                    _ => {}
                }
                println!("FAIL [{kind}] {e}");
            }
        }
    }

    println!();
    println!("Results:");
    println!("  Success: {} / {}", success_count, args.samples);
    println!("  Failed:  {}", failure_count);

    if !latencies.is_empty() {
        latencies.sort();
        let p50 = percentile(&latencies, 50);
        let p95 = percentile(&latencies, 95);
        let avg = latencies.iter().sum::<u64>() as f64 / latencies.len() as f64;
        let avg_tok_sec = if avg > 0.0 {
            total_output_tokens as f64 / (avg * latencies.len() as f64 / 1000.0)
        } else {
            0.0
        };

        println!("  p50 latency: {}ms", p50);
        println!("  p95 latency: {}ms", p95);
        println!("  avg latency:  {:.0}ms", avg);
        println!("  avg output:   {:.0} tok/s", avg_tok_sec);
    }

    println!("  429 count:    {}", ratelimit_count);
    println!("  timeout count: {}", timeout_count);

    if !latencies.is_empty() {
        let p50 = percentile(&latencies, 50);
        let recommendation = if ratelimit_count > 0 || p50 > 120_000 {
            ("free-tier", 1usize, 300u64)
        } else if p50 < 15_000 && ratelimit_count == 0 {
            ("fastest", 32usize, 120u64)
        } else {
            ("balanced", 16usize, 120u64)
        };
        println!();
        println!("Recommendation:");
        println!("  profile:     {}", recommendation.0);
        println!("  concurrency: {}", recommendation.1);
        println!("  timeout:     {}s", recommendation.2);
    }

    Ok(())
}

fn classify_error(e: &LlmError) -> &'static str {
    match e {
        LlmError::Http(http_err) => {
            if http_err.is_timeout() {
                "timeout"
            } else {
                "http"
            }
        }
        LlmError::HttpStatus { status, .. } if *status == 429 => "rate_limit",
        LlmError::HttpStatus { status, .. } if (500..600).contains(status) => "server",
        LlmError::HttpStatus { .. } => "client",
        LlmError::Provider(_) => "provider",
        LlmError::InvalidResponse(_) => "invalid_response",
        LlmError::Json(_) => "json",
    }
}

fn percentile(data: &[u64], pct: usize) -> u64 {
    if data.is_empty() {
        return 0;
    }
    let idx = ((pct as f64 / 100.0) * (data.len() - 1) as f64).round() as usize;
    data[idx.min(data.len() - 1)]
}

#[cfg(test)]
mod tests;
