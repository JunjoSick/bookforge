use super::*;

/// Shared post-translation pipeline: QA, fallback, double-check, finish, report.
/// Both batch and non-batch paths call this after translation completes.
#[allow(clippy::too_many_arguments)]
pub(super) async fn finish_translation_pipeline<P>(
    provider: &P,
    cancel_token: &tokio_util::sync::CancellationToken,
    cli_args: &TranslateArgs,
    segments: &[Segment],
    translations: &mut Vec<SegmentTranslation>,
    store: &JobStore,
    job: &JobRecord,
    run_prompt_version: &str,
    settings: &ResolvedRunSettings,
    run_config: &TranslationRunConfig,
    config: &TranslationConfig,
    rebuild_options: &bookforge_epub::RebuildOptions,
    book: &bookforge_core::ir::Book,
    progress: Arc<dyn bookforge_core::ProgressSink>,
    started: std::time::Instant,
    snapshot: &mut RunConfigSnapshot,
    job_runtime_settings: &tokio::sync::watch::Receiver<crate::control::JobRuntimeSettings>,
    telemetry: &TelemetryLog,
    glossary_rules: &std::collections::HashMap<
        String,
        Vec<bookforge_core::glossary::GlossarySelectionRule>,
    >,
) -> Result<()>
where
    P: LlmProvider + Clone,
{
    translations.sort_by_key(|t| t.ordinal);

    let pause_signal = run_config.pause_signal.clone().unwrap_or_default();
    let mut controlled_run_config = run_config.clone();
    controlled_run_config.pause_signal = Some(pause_signal.clone());
    let mut control_poller = crate::control::ControlFilePoller::new_with_stop_cancel(
        store,
        &job.id,
        progress.clone(),
        cancel_token.clone(),
    );

    if !wait_for_finalize_stage_control(&mut control_poller, &pause_signal).await? {
        print_stopped_resume_hint(&job.id, human_stdout_enabled(cli_args.ui));
        return Ok(());
    }
    let qa_runtime = job_runtime_settings.borrow().clone();
    let qa_run_config =
        crate::control::freeze_run_config_for_stage(&controlled_run_config, &qa_runtime);
    let qa_reviews = qa_reviews_for_mode(
        ProgressRequestProvider::new(provider.clone(), progress.clone()),
        segments,
        translations,
        &qa_run_config,
        &qa_runtime.settings.qa,
        qa_runtime.qa,
    )
    .await;

    if !wait_for_finalize_stage_control(&mut control_poller, &pause_signal).await? {
        print_stopped_resume_hint(&job.id, human_stdout_enabled(cli_args.ui));
        return Ok(());
    }
    let fallback_config = FallbackPassConfig::from_snapshot(snapshot.fallback.as_ref());
    let fallback_translations = run_fallback_pass_instrumented(
        cancel_token,
        fallback_config.as_ref(),
        segments,
        std::mem::take(translations),
        store,
        &job.id,
        run_prompt_version,
        settings,
        &controlled_run_config,
        Some(&mut control_poller),
        progress.clone(),
        telemetry,
        glossary_rules,
    )
    .await?;
    *translations = fallback_translations;
    if job_was_stopped(store, &job.id)? {
        print_stopped_resume_hint(&job.id, human_stdout_enabled(cli_args.ui));
        return Ok(());
    }

    if !wait_for_finalize_stage_control(&mut control_poller, &pause_signal).await? {
        print_stopped_resume_hint(&job.id, human_stdout_enabled(cli_args.ui));
        return Ok(());
    }
    let double_check_runtime = job_runtime_settings.borrow().clone();
    let double_check_run_config =
        crate::control::freeze_run_config_for_stage(&controlled_run_config, &double_check_runtime);
    if !snapshot.finalize.double_check_complete
        && run_double_check_pass(DoubleCheckPass {
            provider,
            cancel_token,
            cli_args,
            segments,
            translations,
            store,
            job_id: &job.id,
            config: &double_check_run_config,
            settings: &double_check_runtime.settings,
            progress: progress.clone(),
        })
        .await?
    {
        snapshot.finalize.double_check_complete = true;
        store.update_job_config_snapshot(&job.id, snapshot)?;
    }
    if job_was_stopped(store, &job.id)? {
        print_stopped_resume_hint(&job.id, human_stdout_enabled(cli_args.ui));
        return Ok(());
    }

    loop {
        if !wait_for_finalize_stage_control(&mut control_poller, &pause_signal).await? {
            print_stopped_resume_hint(&job.id, human_stdout_enabled(cli_args.ui));
            return Ok(());
        }
        if mark_job_finished(store, &job.id, translations)? {
            break;
        }
        if job_was_stopped(store, &job.id)? {
            print_stopped_resume_hint(&job.id, human_stdout_enabled(cli_args.ui));
            return Ok(());
        }
    }
    let validation_runtime = job_runtime_settings.borrow().clone();
    print_summary_rebuild_and_report(
        store,
        job,
        book,
        segments,
        translations,
        &qa_reviews,
        config,
        rebuild_options,
        validation_runtime.validate_output,
        cli_args.strict_epubcheck,
        human_stdout_enabled(cli_args.ui),
    )?;
    let summary = store
        .summary(&job.id)?
        .ok_or_else(|| anyhow::anyhow!("job '{}' summary unavailable", job.id))?;
    reconfigure::clear_overrides_for_job(&job.id)?;
    progress.emit(bookforge_core::ProgressEvent::ArtifactWritten {
        path: config.output.display().to_string(),
        timestamp_ms: bookforge_core::progress::now_ms(),
    });
    progress.emit(bookforge_core::ProgressEvent::TranslationFinished {
        succeeded: summary.succeeded,
        cached: summary.cached,
        needs_review: summary.needs_review,
        failed: summary.failed,
        input_tokens: summary.input_tokens,
        output_tokens: summary.output_tokens,
        elapsed_ms: started.elapsed().as_millis() as u64,
        timestamp_ms: bookforge_core::progress::now_ms(),
    });

    Ok(())
}

pub(super) async fn wait_for_finalize_stage_control(
    control: &mut crate::control::ControlFilePoller<'_>,
    signal: &bookforge_llm::PauseSignal,
) -> Result<bool> {
    if let Some(delay_ms) = std::env::var("BOOKFORGE_TEST_FINALIZE_BOUNDARY_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
    }
    Ok(!matches!(
        control.wait_until_running_or_stopped(signal).await?,
        bookforge_llm::PauseState::Stopped
    ))
}

struct DoubleCheckPass<'a, P> {
    provider: &'a P,
    cancel_token: &'a tokio_util::sync::CancellationToken,
    cli_args: &'a TranslateArgs,
    segments: &'a [Segment],
    translations: &'a mut [SegmentTranslation],
    store: &'a JobStore,
    job_id: &'a str,
    config: &'a TranslationRunConfig,
    settings: &'a ResolvedRunSettings,
    progress: Arc<dyn bookforge_core::ProgressSink>,
}

#[derive(Clone)]
enum DoubleCheckProvider<P> {
    Primary(P),
    Mock(MockProvider),
    OpenAi(OpenAiCompatibleProvider),
}

impl<P> LlmProvider for DoubleCheckProvider<P>
where
    P: LlmProvider,
{
    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> std::result::Result<CompletionResponse, LlmError> {
        match self {
            Self::Primary(provider) => provider.complete(request).await,
            Self::Mock(provider) => provider.complete(request).await,
            Self::OpenAi(provider) => provider.complete(request).await,
        }
    }

    fn capabilities(&self) -> ProviderCapabilities {
        match self {
            Self::Primary(provider) => provider.capabilities(),
            Self::Mock(provider) => provider.capabilities(),
            Self::OpenAi(provider) => provider.capabilities(),
        }
    }

    fn is_reasoning(&self) -> bool {
        match self {
            Self::Primary(provider) => provider.is_reasoning(),
            Self::Mock(provider) => provider.is_reasoning(),
            Self::OpenAi(provider) => provider.is_reasoning(),
        }
    }
}

async fn run_double_check_pass<P>(pass: DoubleCheckPass<'_, P>) -> Result<bool>
where
    P: LlmProvider + Clone,
{
    let DoubleCheckPass {
        provider,
        cancel_token,
        cli_args,
        segments,
        translations,
        store,
        job_id,
        config,
        settings,
        progress,
    } = pass;
    if settings.double_check.mode == DoubleCheckMode::Off {
        return Ok(true);
    }

    let (dc_provider, dc_provider_name, dc_model) =
        if cli_args.double_check_provider.is_some() || cli_args.double_check_model.is_some() {
            let provider_str = cli_args
                .double_check_provider
                .as_deref()
                .unwrap_or("openrouter");
            if provider_str == "mock" {
                let model = cli_args
                    .double_check_model
                    .clone()
                    .unwrap_or_else(|| config.model.clone());
                (
                    DoubleCheckProvider::Mock(MockProvider::new(
                        mock_mode(&model),
                        &config.target_language,
                    )),
                    provider_str.to_string(),
                    model,
                )
            } else {
                let dc_config = provider_config(
                    provider_str,
                    cli_args.double_check_model.as_deref(),
                    cli_args.double_check_base_url.as_deref(),
                    cli_args.double_check_api_key_env.as_deref(),
                    settings.provider.timeout_seconds,
                    settings.provider.provider_max_attempts,
                    settings.provider.thinking_disabled,
                    settings.provider.retry_after_policy,
                    settings.provider.max_backoff_seconds,
                    settings.provider.max_idle_per_host,
                    settings.provider.json_mode,
                )?;
                let provider =
                    OpenAiCompatibleProvider::new_with_cancel(dc_config, cancel_token.clone())
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                let model = provider.model().to_string();
                (
                    DoubleCheckProvider::OpenAi(provider),
                    provider_str.to_string(),
                    model,
                )
            }
        } else {
            (
                DoubleCheckProvider::Primary(provider.clone()),
                config.provider.clone(),
                config.model.clone(),
            )
        };
    let mut double_check_config = config.clone();
    double_check_config.provider = dc_provider_name;
    double_check_config.model = dc_model;

    println!("Double-check: auditing translations...");
    let corrections = match run_double_check(
        ProgressRequestProvider::new(dc_provider, progress),
        segments,
        translations,
        &double_check_config,
        &settings.double_check,
    )
    .await
    {
        Ok(corrections) => corrections,
        Err(_)
            if config
                .pause_signal
                .as_ref()
                .is_some_and(bookforge_llm::PauseSignal::is_stopped) =>
        {
            return Ok(false);
        }
        Err(e) => return Err(anyhow::anyhow!("double-check failed: {e}")),
    };

    let applied = corrections
        .iter()
        .filter(|c| matches!(c.status, bookforge_llm::CorrectionStatus::Applied))
        .count();
    let rejected = corrections
        .iter()
        .filter(|c| {
            matches!(
                c.status,
                bookforge_llm::CorrectionStatus::RejectedValidationFailed(_)
            )
        })
        .count();
    let unresolved = corrections
        .iter()
        .filter(|c| matches!(c.status, bookforge_llm::CorrectionStatus::Unresolved))
        .count();

    let changed_segment_ids = apply_double_check_corrections(translations, &corrections);
    persist_corrected_translations(store, job_id, config, translations, &changed_segment_ids)?;

    println!(
        "  Corrections: {applied} applied, {rejected} rejected, {unresolved} unresolved, {} segments updated",
        changed_segment_ids.len()
    );

    Ok(true)
}

pub(crate) fn apply_double_check_corrections(
    translations: &mut [SegmentTranslation],
    corrections: &[bookforge_llm::CorrectionRecord],
) -> Vec<String> {
    let mut changed_segment_ids = std::collections::BTreeSet::new();

    for correction in corrections {
        if !matches!(correction.status, bookforge_llm::CorrectionStatus::Applied) {
            continue;
        }
        let Some(corrected) = correction.corrected_translation.as_deref() else {
            continue;
        };
        let Some(translation) = translations
            .iter_mut()
            .find(|translation| translation.segment_id == correction.segment_id)
        else {
            continue;
        };
        let Some(block) = translation
            .blocks
            .iter_mut()
            .find(|block| block.block_id == correction.block_id)
        else {
            continue;
        };
        if block.text != corrected {
            block.text = corrected.to_string();
            changed_segment_ids.insert(translation.segment_id.0.clone());
        }
    }

    changed_segment_ids.into_iter().collect()
}

pub(crate) fn persist_corrected_translations(
    store: &JobStore,
    job_id: &str,
    config: &TranslationRunConfig,
    translations: &[SegmentTranslation],
    changed_segment_ids: &[String],
) -> Result<()> {
    for segment_id in changed_segment_ids {
        let Some(translation) = translations
            .iter()
            .find(|translation| translation.segment_id.0 == *segment_id)
        else {
            continue;
        };
        let joined = translation.joined_text();
        store.save_translation(SaveTranslation {
            job_id,
            segment_id: &translation.segment_id.0,
            translated_text: &joined,
            blocks: &translation.blocks,
            provider: &config.provider,
            model: &config.model,
            prompt_version: &config.prompt_version,
            input_tokens: translation.input_tokens,
            input_cached_tokens: translation.input_cached_tokens,
            output_tokens: translation.output_tokens,
            tokens_estimated: translation.tokens_estimated,
        })?;
    }

    Ok(())
}

pub(crate) async fn qa_reviews_for_mode<P>(
    provider: P,
    segments: &[Segment],
    translations: &[SegmentTranslation],
    config: &TranslationRunConfig,
    qa_config: &bookforge_core::config::QaRunConfig,
    qa_mode: QaMode,
) -> Vec<QaSegmentReview>
where
    P: LlmProvider,
{
    match qa_mode {
        QaMode::Off => Vec::new(),
        QaMode::All => {
            qa_segments_parallel(provider, segments, translations, config, qa_config).await
        }
        QaMode::Suspicious => {
            let candidates = suspicious_qa_candidates(segments, translations);
            qa_segments_parallel(provider, segments, &candidates, config, qa_config).await
        }
    }
}

pub(super) fn suspicious_qa_candidates(
    segments: &[Segment],
    translations: &[SegmentTranslation],
) -> Vec<SegmentTranslation> {
    let by_segment = segments
        .iter()
        .map(|segment| (segment.id.0.as_str(), segment))
        .collect::<std::collections::HashMap<_, _>>();
    translations
        .iter()
        .filter(|translation| {
            matches!(
                translation.status,
                SegmentStatus::Succeeded | SegmentStatus::SkippedCached
            )
        })
        .filter(|translation| {
            let Some(segment) = by_segment.get(translation.segment_id.0.as_str()) else {
                return false;
            };
            let source_len = segment.source.text.chars().count().max(1);
            let translated_len = translation.joined_text().chars().count();
            let ratio = translated_len as f64 / source_len as f64;
            !(0.5..=2.2).contains(&ratio)
                || translation.template == "translate_run_preserving"
                || segment.constraints.preserve_spans.len() >= 4
                || marker_structure_changed(segment, translation)
        })
        .cloned()
        .collect()
}

#[derive(Debug, Clone, Eq, Ord, PartialEq, PartialOrd)]
struct MarkerSignature {
    block_index: usize,
    id: String,
    shape: MarkerShape,
}

#[derive(Debug, Clone, Eq, Ord, PartialEq, PartialOrd)]
enum MarkerShape {
    PairedM,
    PairedKeep,
    EmptyRef,
}

fn marker_structure_changed(segment: &Segment, translation: &SegmentTranslation) -> bool {
    let Some(mut expected) = marker_signatures_for_blocks(
        segment
            .source
            .blocks
            .iter()
            .map(|block| block.text.as_str()),
    ) else {
        return true;
    };
    let Some(mut actual) =
        marker_signatures_for_blocks(translation.blocks.iter().map(|block| block.text.as_str()))
    else {
        return true;
    };
    expected.sort();
    actual.sort();
    expected != actual
}

fn marker_signatures_for_blocks<'a>(
    blocks: impl Iterator<Item = &'a str>,
) -> Option<Vec<MarkerSignature>> {
    let mut signatures = Vec::new();
    for (block_index, text) in blocks.enumerate() {
        signatures.extend(marker_signatures_in_text(block_index, text)?);
    }
    Some(signatures)
}

fn marker_signatures_in_text(block_index: usize, text: &str) -> Option<Vec<MarkerSignature>> {
    let mut signatures = Vec::new();
    let mut open_stack: Vec<String> = Vec::new();
    let mut rest = text;

    while let Some(index) = rest.find('<') {
        let tag = &rest[index..];
        if let Some(open) = parse_paired_marker_open(tag) {
            let shape = if open.tag_name == "keep" {
                MarkerShape::PairedKeep
            } else {
                MarkerShape::PairedM
            };
            signatures.push(MarkerSignature {
                block_index,
                id: open.id,
                shape,
            });
            open_stack.push(open.tag_name);
            rest = &tag[open.len..];
        } else if let Some(empty) = parse_empty_marker(tag) {
            signatures.push(MarkerSignature {
                block_index,
                id: empty.id,
                shape: MarkerShape::EmptyRef,
            });
            rest = &tag[empty.len..];
        } else if let Some(close) = parse_marker_close(tag) {
            if open_stack.pop().as_deref() != Some(close.tag_name.as_str()) {
                return None;
            }
            rest = &tag[close.len..];
        } else {
            rest = &tag[1..];
        }
    }

    if open_stack.is_empty() {
        Some(signatures)
    } else {
        None
    }
}

pub(super) fn mark_unfinished_segments_failed(
    store: &JobStore,
    job_id: &str,
    segments: &[Segment],
    error: &str,
) -> Result<()> {
    let segment_ids = segments
        .iter()
        .map(|segment| segment.id.0.clone())
        .collect::<Vec<_>>();
    store.mark_unfinished_segments_failed(job_id, &segment_ids, error)?;
    Ok(())
}

pub(crate) fn mark_job_finished(
    store: &JobStore,
    job_id: &str,
    translations: &[SegmentTranslation],
) -> Result<bool> {
    if job_was_stopped(store, job_id)? || job_is_paused(store, job_id)? {
        return Ok(false);
    }
    let Some(summary) = store.summary(job_id)? else {
        anyhow::bail!("job '{job_id}' was not found");
    };
    let terminal_segments =
        summary.succeeded + summary.cached + summary.failed + summary.needs_review;
    if terminal_segments < summary.total_segments || summary.retry_pending > 0 {
        store.mark_job_needs_review(job_id)?;
        return Ok(!job_was_stopped(store, job_id)? && !job_is_paused(store, job_id)?);
    }
    if translations
        .iter()
        .any(|translation| translation.status == SegmentStatus::Failed)
    {
        store.mark_job_needs_review(job_id)?;
        return Ok(!job_was_stopped(store, job_id)? && !job_is_paused(store, job_id)?);
    }
    if translations
        .iter()
        .any(|translation| translation.status == SegmentStatus::NeedsReview)
    {
        store.mark_job_needs_review(job_id)?;
        return Ok(!job_was_stopped(store, job_id)? && !job_is_paused(store, job_id)?);
    }
    store.mark_job_complete(job_id)?;
    Ok(!job_was_stopped(store, job_id)? && !job_is_paused(store, job_id)?)
}

pub(crate) fn job_was_stopped(store: &JobStore, job_id: &str) -> Result<bool> {
    Ok(store
        .get_job(job_id)?
        .is_some_and(|job| job.status == "stopped"))
}

fn job_is_paused(store: &JobStore, job_id: &str) -> Result<bool> {
    Ok(store
        .get_job(job_id)?
        .is_some_and(|job| job.status == "paused"))
}

pub(crate) fn print_stopped_resume_hint(job_id: &str, print_stdout: bool) {
    if print_stdout {
        println!("Stopped. Progress has been saved to job: {job_id}");
        println!("Resume with: bookforge resume {job_id}");
    }
}
