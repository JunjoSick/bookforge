use super::*;

pub(super) fn repair_batch_item_limit(target_language: &str) -> usize {
    if target_language.trim().eq_ignore_ascii_case("Toki Pona") {
        1
    } else {
        16
    }
}

enum BatchWorkerResult {
    Provider(Result<BatchTranslationResult, LlmError>),
    StoppedUnfinished,
}

struct BatchWorkerOutput {
    batch: TranslationBatch,
    result: BatchWorkerResult,
    request_status: RequestStatus,
    finish_reason: Option<FinishReason>,
    returned_items: Option<usize>,
    latency_ms: u64,
    max_output_tokens: u32,
    output_escalated: bool,
    next_max_output_tokens: Option<u32>,
    request_permit: Option<AdaptivePermit>,
}

struct RepairWorkerOutput {
    batch: TranslationBatch,
    result: Result<BatchTranslationResult, LlmError>,
    finish_reason: Option<FinishReason>,
    returned_items: Option<usize>,
    latency_ms: u64,
    max_output_tokens: u32,
}

struct BatchRequestOutput {
    result: Result<BatchTranslationResult, LlmError>,
    finish_reason: Option<FinishReason>,
    returned_items: Option<usize>,
}

struct InFlightRequestGuard(Arc<AtomicUsize>);

impl InFlightRequestGuard {
    fn start(counter: Arc<AtomicUsize>) -> (Self, usize) {
        let active = counter.fetch_add(1, Ordering::SeqCst) + 1;
        (Self(counter), active)
    }
}

impl Drop for InFlightRequestGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn is_transient(err: &LlmError) -> bool {
    match err {
        LlmError::HttpStatus { status, .. } => *status == 429 || *status >= 500,
        LlmError::Http(e) => e.is_timeout() || e.is_connect() || e.is_decode() || e.is_body(),
        _ => false,
    }
}

pub fn collect_repair_items(result: &BatchTranslationResult) -> Vec<TranslationBatchItem> {
    result
        .failures
        .iter()
        .map(|f| TranslationBatchItem {
            item_id: f.item_id.clone(),
            segment_id: f.segment_id.clone(),
            // Repair items don't participate in the sliding-context fence
            // — they're JSON-syntax fixups, not new translation work. The
            // sentinel empty section_id is intentional and safe; the
            // batch driver never awaits context for Repair-kind batches.
            section_id: bookforge_core::ir::SectionId(String::new()),
            block_id: bookforge_core::ir::BlockId(String::new()),
            ordinal: 0,
            kind: String::new(),
            source_text: String::new(),
            text_runs: Vec::new(),
            protected_spans: Vec::new(),
            required_markers: Vec::new(),
            checksum: String::new(),
        })
        .collect()
}

/// Publish Failed status for each segment in a batch that hit a terminal
/// error. This unblocks the sliding-context fence so downstream batches
/// don't deadlock waiting on a segment that will never succeed.
fn unblock_fence_for_batch_failure(
    registry: Option<&crate::scheduler::ContextRegistry>,
    segments_by_id: &HashMap<String, Segment>,
    items: &[TranslationBatchItem],
) {
    let Some(registry) = registry else { return };
    let mut seen = std::collections::HashSet::<String>::new();
    for item in items {
        let key = item.segment_id.0.clone();
        if !seen.insert(key.clone()) {
            continue;
        }
        if let Some(segment) = segments_by_id.get(&key) {
            registry.pre_populate_text(segment, String::new(), SegmentStatus::Failed);
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn translate_batches_with_callback<P, F>(
    provider: P,
    batches: Vec<TranslationBatch>,
    segments: &[Segment],
    config: &TranslationRunConfig,
    telemetry: Arc<TelemetryLog>,
    rate_controller: Option<Arc<ProviderRateController>>,
    batch_sizer: Option<&mut BatchSizer>,
    progress: Arc<dyn bookforge_core::ProgressSink>,
    finalized_tx: Option<mpsc::Sender<SegmentTranslation>>,
    on_segment: F,
) -> Result<Vec<SegmentTranslation>, LlmError>
where
    P: LlmProvider,
    F: FnMut(&SegmentTranslation) -> Result<(), LlmError>,
{
    translate_batches_with_control(
        provider,
        batches,
        segments,
        config,
        telemetry,
        rate_controller,
        batch_sizer,
        progress,
        finalized_tx,
        on_segment,
        |_| Ok(()),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn translate_batches_with_control<P, F, C>(
    provider: P,
    batches: Vec<TranslationBatch>,
    segments: &[Segment],
    config: &TranslationRunConfig,
    telemetry: Arc<TelemetryLog>,
    rate_controller: Option<Arc<ProviderRateController>>,
    mut batch_sizer: Option<&mut BatchSizer>,
    progress: Arc<dyn bookforge_core::ProgressSink>,
    finalized_tx: Option<mpsc::Sender<SegmentTranslation>>,
    mut on_segment: F,
    mut on_control_boundary: C,
) -> Result<Vec<SegmentTranslation>, LlmError>
where
    P: LlmProvider,
    F: FnMut(&SegmentTranslation) -> Result<(), LlmError>,
    C: FnMut(&PauseSignal) -> Result<(), LlmError>,
{
    let library = Arc::new(PromptLibrary::global().clone());
    let provider = Arc::new(provider);
    let validate_source_copy = crate::validation::should_validate_source_copy(
        &config.provider,
        config.source_language.as_deref(),
        &config.target_language,
    );
    let section_titles = Arc::new(
        segments
            .iter()
            .filter_map(|segment| {
                segment
                    .metadata
                    .section_title
                    .as_ref()
                    .map(|title| (segment.id.0.clone(), title.clone()))
            })
            .collect::<HashMap<_, _>>(),
    );
    let config = Arc::new(config.clone());
    let pause_signal = config.pause_signal.clone();
    let initial_concurrency = config.scheduler.concurrency.max(1);
    let request_limiter = Arc::new(AdaptiveLimiter::new_with_bounds(
        initial_concurrency,
        1,
        Semaphore::MAX_PERMITS,
        Duration::ZERO,
        Some(progress.clone()),
    ));
    let active_requests = Arc::new(AtomicUsize::new(0));

    let all_items: HashMap<String, TranslationBatchItem> = batches
        .iter()
        .flat_map(|b| b.items.iter())
        .map(|item| (item.item_id.clone(), item.clone()))
        .collect();

    // Sliding-context fence (PR5): publish per-segment as soon as all of a
    // segment's blocks have arrived from one or more batches. We buffer
    // per-segment blocks until the count matches the expected block count,
    // then push the joined text into the context registry so later batches
    // can read it as prior context.
    let segments_by_id: HashMap<String, Segment> = segments
        .iter()
        .map(|s| (s.id.0.clone(), s.clone()))
        .collect();
    let segment_block_expected: HashMap<String, usize> = segments
        .iter()
        .map(|s| (s.id.0.clone(), s.block_ids.len()))
        .collect();
    let mut pending_segment_translations: HashMap<String, SegmentTranslation> = HashMap::new();
    let mut incrementally_finalized_segment_ids = std::collections::HashSet::<String>::new();

    let mut all_results: Vec<BatchTranslationResult> = Vec::new();
    let mut pending: Vec<TranslationBatch> = batches;
    let max_rounds = 3usize;
    let mut single_invalid_attempts_by_item: HashMap<String, usize> = HashMap::new();
    let mut transient_attempts_by_item: HashMap<String, usize> = HashMap::new();
    let mut escalated_output_tokens_by_item: HashMap<String, u32> = HashMap::new();
    let mut compact_retry_attempts_by_item: HashMap<String, usize> = HashMap::new();
    let mut truncation_alert = TruncationAlertState::default();
    let mut stop_dispatch = false;
    let mut runtime_sizer: Option<(u64, bool, BatchSizer)> = None;
    let mut repartitioned_revision: Option<u64> = None;

    for _round in 0..max_rounds {
        if pending.is_empty() || stop_dispatch {
            break;
        }

        // Spawn one task per queued batch, but gate provider calls below with
        // request_semaphore. Context waiters must not consume provider
        // concurrency, otherwise a split prerequisite batch can be stranded
        // behind later batches that are waiting for its context.
        let mut pending_queue: VecDeque<TranslationBatch> = pending.drain(..).collect();
        let mut tasks = JoinSet::<BatchWorkerOutput>::new();

        while (!pending_queue.is_empty() && !stop_dispatch) || !tasks.is_empty() {
            while !pending_queue.is_empty() && !stop_dispatch {
                if let Some(signal) = pause_signal.as_ref() {
                    on_control_boundary(signal)?;
                    match signal.state() {
                        PauseState::Running => {}
                        PauseState::Paused => break,
                        PauseState::Stopped => {
                            stop_dispatch = true;
                            break;
                        }
                    }
                }

                let runtime_snapshot = config
                    .runtime_settings
                    .as_ref()
                    .map(|receiver| receiver.borrow().clone());
                if let Some(runtime) = runtime_snapshot.as_ref() {
                    if request_limiter.current() != runtime.concurrency {
                        request_limiter.set_target(runtime.concurrency.max(1), "runtime_config");
                    }
                    if runtime.revision > 0
                        && runtime_sizer
                            .as_ref()
                            .is_none_or(|(revision, _, _)| *revision != runtime.revision)
                    {
                        runtime_sizer = Some((
                            runtime.revision,
                            runtime.batch.adaptive_sizing,
                            BatchSizer::with_progress(
                                runtime.batch.target_tokens,
                                runtime.batch.max_items,
                                progress.clone(),
                            ),
                        ));
                    }
                    if runtime.revision > 0
                        && repartitioned_revision != Some(runtime.revision)
                        && let Some((_, _, sizer)) = runtime_sizer.as_ref()
                    {
                        repartition_pending_batches(
                            &mut pending_queue,
                            sizer,
                            Some(config.as_ref()),
                            runtime.revision,
                        );
                        repartitioned_revision = Some(runtime.revision);
                    }
                }

                let dispatch_concurrency = runtime_snapshot
                    .as_ref()
                    .map(|runtime| runtime.concurrency)
                    .unwrap_or(config.scheduler.concurrency)
                    .max(1);
                if tasks.len() >= dispatch_concurrency {
                    break;
                }

                let Some(batch) = pending_queue.pop_front() else {
                    break;
                };
                let pending_output_override =
                    take_batch_output_override(&mut escalated_output_tokens_by_item, &batch);
                let active_sizer = runtime_sizer
                    .as_ref()
                    .map(|(_, _, sizer)| sizer)
                    .or(batch_sizer.as_deref());
                let mut normalized =
                    normalize_batch_for_current_sizer(batch, active_sizer, Some(config.as_ref()));
                if let Some(output_override) = pending_output_override {
                    for part in &normalized {
                        set_batch_output_override(
                            &mut escalated_output_tokens_by_item,
                            part,
                            output_override,
                        );
                    }
                }
                let batch = normalized.remove(0);
                let output_override =
                    take_batch_output_override(&mut escalated_output_tokens_by_item, &batch);
                let compact_retry_attempt = batch
                    .items
                    .iter()
                    .filter_map(|item| compact_retry_attempts_by_item.get(&item.item_id).copied())
                    .max()
                    .unwrap_or(0);
                for extra in normalized.into_iter().rev() {
                    pending_queue.push_front(extra);
                }
                progress.emit(bookforge_core::ProgressEvent::BatchQueued {
                    batch_id: batch.id.clone(),
                    item_count: batch.items.len(),
                    timestamp_ms: bookforge_core::progress::now_ms(),
                });

                let provider = provider.clone();
                let library = library.clone();
                let config = config.clone();
                let runtime_settings = config.runtime_settings.clone();
                let rate_controller = rate_controller.clone();
                let progress = progress.clone();
                let request_limiter = request_limiter.clone();
                let section_titles = section_titles.clone();
                let pause_signal = pause_signal.clone();
                let active_requests = active_requests.clone();

                tasks.spawn(async move {
                    let output_escalated = output_override.is_some();
                    // Strict context must be awaited before any permit is
                    // held (waiters would starve prerequisite batches);
                    // best-effort context is snapshotted after permits so
                    // earlier batches have had time to publish.
                    let strict_context_pairs = if config.context.strict {
                        Some(context_pairs_for_batch(&batch, &config).await)
                    } else {
                        None
                    };
                    if let Some(signal) = pause_signal.as_ref()
                        && signal.wait_until_running_or_stopped().await == PauseState::Stopped
                    {
                        return BatchWorkerOutput {
                            batch,
                            result: BatchWorkerResult::StoppedUnfinished,
                            request_status: RequestStatus::OtherError,
                            finish_reason: None,
                            returned_items: None,
                            latency_ms: 0,
                            max_output_tokens: 0,
                            output_escalated,
                            next_max_output_tokens: None,
                            request_permit: None,
                        };
                    }
                    let request_permit = loop {
                        if let Some(receiver) = runtime_settings.as_ref() {
                            let target = receiver.borrow().concurrency.max(1);
                            if request_limiter.current() != target {
                                request_limiter.set_target(target, "runtime_config");
                            }
                        }
                        if let Some(signal) = pause_signal.as_ref()
                            && signal.wait_until_running_or_stopped().await == PauseState::Stopped
                        {
                            return BatchWorkerOutput {
                                batch,
                                result: BatchWorkerResult::StoppedUnfinished,
                                request_status: RequestStatus::OtherError,
                                finish_reason: None,
                                returned_items: None,
                                latency_ms: 0,
                                max_output_tokens: 0,
                                output_escalated,
                                next_max_output_tokens: None,
                                request_permit: None,
                            };
                        }
                        match request_limiter.try_acquire() {
                            Ok(permit) => break permit,
                            Err(TryAcquireError::NoPermits) => {
                                tokio::time::sleep(Duration::from_millis(25)).await;
                            }
                            Err(TryAcquireError::Closed) => {
                                return BatchWorkerOutput {
                                    batch,
                                    result: BatchWorkerResult::Provider(Err(LlmError::Provider(
                                        "batch request semaphore closed".to_string(),
                                    ))),
                                    request_status: RequestStatus::OtherError,
                                    finish_reason: None,
                                    returned_items: None,
                                    latency_ms: 0,
                                    max_output_tokens: 0,
                                    output_escalated,
                                    next_max_output_tokens: None,
                                    request_permit: None,
                                };
                            }
                        }
                    };

                    let adaptive_concurrency = runtime_settings
                        .as_ref()
                        .map(|receiver| receiver.borrow().adaptive_concurrency)
                        // Existing library callers express the enabled state by
                        // supplying a controller and no runtime receiver.
                        .unwrap_or_else(|| rate_controller.is_some());
                    let permit = match rate_controller.as_ref().filter(|_| adaptive_concurrency) {
                        Some(controller) => match controller.acquire().await {
                            Ok(permit) => Some(permit),
                            Err(_) => {
                                return BatchWorkerOutput {
                                    batch,
                                    result: BatchWorkerResult::Provider(Err(LlmError::Provider(
                                        "adaptive concurrency limiter closed".to_string(),
                                    ))),
                                    request_status: RequestStatus::OtherError,
                                    finish_reason: None,
                                    returned_items: None,
                                    latency_ms: 0,
                                    max_output_tokens: 0,
                                    output_escalated,
                                    next_max_output_tokens: None,
                                    request_permit: Some(request_permit),
                                };
                            }
                        },
                        None => None,
                    };

                    let mut effective_config = config.as_ref().clone();
                    if let Some(receiver) = runtime_settings.as_ref() {
                        let runtime = receiver.borrow().clone();
                        effective_config.scheduler.concurrency = runtime.concurrency.max(1);
                        effective_config.batch_max_output_tokens = runtime.batch_max_output_tokens;
                        effective_config.runtime_settings = Some(runtime.frozen_receiver());
                    }
                    let config = Arc::new(effective_config);

                    let context_pairs = match strict_context_pairs {
                        Some(pairs) => pairs,
                        None => context_pairs_for_batch(&batch, &config).await,
                    };

                    let started = std::time::Instant::now();
                    let is_reasoning = provider.is_reasoning();
                    let default_max_output_tokens =
                        capped_batch_max_output_tokens(&batch, &config, is_reasoning);
                    let max_output_tokens = output_override.unwrap_or(default_max_output_tokens);
                    let next_max_output_tokens = (!output_escalated)
                        .then(|| {
                            next_escalated_batch_max_output_tokens(
                                max_output_tokens,
                                &batch,
                                &config,
                                is_reasoning,
                            )
                        })
                        .flatten();

                    let request_id = format!("batch_{}", batch.id);
                    let (runtime_config_revision, provider_max_attempts) =
                        config.request_runtime_metadata();
                    let (_in_flight, active_request_count) =
                        InFlightRequestGuard::start(active_requests);
                    progress.emit(bookforge_core::ProgressEvent::RequestStarted {
                        request_id: request_id.clone(),
                        batch_id: Some(batch.id.clone()),
                        segment_id: None,
                        provider: Some(config.provider.clone()),
                        model: Some(config.model.clone()),
                        prompt_template: None,
                        items: batch.items.len(),
                        estimated_input_tokens: batch.token_estimate,
                        max_output_tokens: Some(max_output_tokens),
                        active_requests: active_request_count,
                        target_concurrency: config.scheduler.concurrency,
                        runtime_config_revision,
                        provider_max_attempts,
                        timestamp_ms: bookforge_core::progress::now_ms(),
                    });

                    let attempt = translate_one_batch_with_evidence(
                        provider.clone(),
                        library.clone(),
                        batch.clone(),
                        BatchTranslationRequest {
                            config: &config,
                            max_output_tokens_override: Some(max_output_tokens),
                            context_pairs,
                            validate_source_copy,
                            section_titles: &section_titles,
                            compact_retry_attempt,
                        },
                    )
                    .await;
                    let latency_ms = started.elapsed().as_millis() as u64;

                    let request_status = request_status_for_controller(&attempt.result);

                    drop(permit);
                    BatchWorkerOutput {
                        batch,
                        result: BatchWorkerResult::Provider(attempt.result),
                        request_status,
                        finish_reason: attempt.finish_reason,
                        returned_items: attempt.returned_items,
                        latency_ms,
                        max_output_tokens,
                        output_escalated,
                        next_max_output_tokens,
                        request_permit: Some(request_permit),
                    }
                });
            }

            if tasks.is_empty() {
                if stop_dispatch || pending_queue.is_empty() {
                    continue;
                }
                if let Some(signal) = pause_signal.as_ref()
                    && signal.state() == PauseState::Paused
                    && wait_for_batch_resume_or_stop(signal, &mut on_control_boundary).await?
                        == PauseState::Stopped
                {
                    stop_dispatch = true;
                }
                continue;
            }

            let joined = match pause_signal.as_ref() {
                Some(signal) if signal.state() == PauseState::Paused => {
                    tokio::select! {
                        joined = tasks.join_next() => joined,
                        state = wait_for_batch_resume_or_stop(signal, &mut on_control_boundary) => {
                            if state? == PauseState::Stopped {
                                stop_dispatch = true;
                            }
                            tasks.join_next().await
                        }
                    }
                }
                _ => tasks.join_next().await,
            };
            let Some(joined) = joined else {
                continue;
            };
            let BatchWorkerOutput {
                batch,
                result,
                request_status,
                finish_reason,
                returned_items,
                latency_ms,
                max_output_tokens,
                output_escalated,
                next_max_output_tokens,
                request_permit,
            } = joined
                .map_err(|err| LlmError::Provider(format!("batch worker task failed: {err}")))?;

            if let Some(signal) = pause_signal.as_ref() {
                on_control_boundary(signal)?;
                if signal.state() == PauseState::Stopped {
                    stop_dispatch = true;
                }
            }
            drop(request_permit);

            let result = match result {
                BatchWorkerResult::Provider(result) => result,
                BatchWorkerResult::StoppedUnfinished => {
                    stop_dispatch = true;
                    unblock_fence_for_batch_failure(
                        config.context_registry.as_deref(),
                        &segments_by_id,
                        &batch.items,
                    );
                    continue;
                }
            };
            let recorded_status =
                batch_request_metric_status(&result, batch.items.len(), returned_items);
            let recorded_finish_reason =
                finish_reason.map(|reason| finish_reason_label(reason).to_string());

            progress.emit(bookforge_core::ProgressEvent::RequestFinished {
                request_id: format!("batch_{}", batch.id),
                batch_id: Some(batch.id.clone()),
                segment_id: None,
                status: recorded_status.clone(),
                latency_ms,
                status_code: None,
                finish_reason: recorded_finish_reason.clone(),
                retry_count: 0,
                input_tokens: result.as_ref().ok().and_then(|r| r.input_tokens),
                output_tokens: result.as_ref().ok().and_then(|r| r.output_tokens),
                error_kind: result.as_ref().err().map(|e| format!("{e:?}")),
                timestamp_ms: bookforge_core::progress::now_ms(),
            });

            telemetry.record(ProviderRequestMetric {
                request_id: format!("batch_{}", batch.id),
                batch_id: Some(batch.id.clone()),
                provider: config.provider.clone(),
                model: config.model.clone(),
                profile: config.profile.namespace_str().to_string(),
                items: batch.items.len(),
                estimated_input_tokens: batch.token_estimate,
                max_output_tokens: Some(max_output_tokens),
                input_tokens: result.as_ref().ok().and_then(|r| r.input_tokens),
                output_tokens: result.as_ref().ok().and_then(|r| r.output_tokens),
                latency_ms,
                finish_reason: recorded_finish_reason,
                status: recorded_status,
                status_code: None,
                retry_count: 0,
                backoff_ms: 0,
                error_kind: None,
            });

            let adaptive_concurrency = config
                .runtime_settings
                .as_ref()
                .map(|receiver| receiver.borrow().adaptive_concurrency)
                .unwrap_or_else(|| rate_controller.is_some());
            if let Some(controller) = rate_controller.as_ref().filter(|_| adaptive_concurrency) {
                controller.observe(request_status, latency_ms);
            }

            match result {
                Ok(batch_result) => {
                    for item in &batch.items {
                        compact_retry_attempts_by_item.remove(&item.item_id);
                    }
                    truncation_alert.observe_resolved();
                    if let Some(sizer) =
                        adaptive_sizer_mut(&mut runtime_sizer, batch_sizer.as_deref_mut())
                    {
                        sizer.on_success_for_mode(batch.mode, latency_ms);
                    }
                    // Publish completed segments to the context registry as
                    // soon as all of their blocks have landed. Repair batches
                    // don't participate (they're fixing prior translations,
                    // not producing new ones — and they carry the sentinel
                    // empty section_id).
                    if batch.kind == BatchKind::Translation {
                        for item in &batch_result.translations {
                            let key = item.segment_id.0.clone();
                            let Some(source_item) = all_items.get(&item.item_id) else {
                                continue;
                            };
                            let Some(segment) = segments_by_id.get(&key) else {
                                continue;
                            };
                            let entry = pending_segment_translations
                                .entry(key.clone())
                                .or_insert_with(|| SegmentTranslation {
                                    segment_id: segment.id.clone(),
                                    ordinal: segment.ordinal,
                                    block_ids: segment.block_ids.clone(),
                                    blocks: Vec::new(),
                                    checksum: segment.checksum.clone(),
                                    status: SegmentStatus::Succeeded,
                                    template: "batch".to_string(),
                                    error: None,
                                    input_tokens: None,
                                    input_cached_tokens: None,
                                    output_tokens: None,
                                    tokens_estimated: false,
                                });
                            add_usage(entry, item);
                            if let Some(warning) = &item.warning {
                                append_translation_error(entry, warning);
                            }
                            if !entry
                                .blocks
                                .iter()
                                .any(|block| block.block_id == source_item.block_id)
                            {
                                entry.blocks.push(BlockTranslation {
                                    block_id: source_item.block_id.clone(),
                                    text: item.text.clone(),
                                });
                            }
                            let expected = segment_block_expected
                                .get(&key)
                                .copied()
                                .unwrap_or(usize::MAX);
                            if entry.blocks.len() >= expected {
                                let mut completed = pending_segment_translations
                                    .remove(&key)
                                    .expect("completed segment accumulator");
                                let (blocks, missing, extra, duplicate) = order_blocks_by_segment(
                                    &completed.block_ids,
                                    std::mem::take(&mut completed.blocks),
                                );
                                if missing.is_empty() && extra.is_empty() && duplicate.is_empty() {
                                    completed.blocks = blocks;
                                    if let Some(registry) = config.context_registry.as_deref() {
                                        let joined = completed
                                            .blocks
                                            .iter()
                                            .map(|block| block.text.as_str())
                                            .collect::<Vec<_>>()
                                            .join("\n\n");
                                        registry.pre_populate_text(
                                            segment,
                                            joined,
                                            SegmentStatus::Succeeded,
                                        );
                                    }
                                    if let Some(ref tx) = finalized_tx {
                                        tx.send(completed.clone()).await.map_err(|_| {
                                            LlmError::Provider(
                                                "finalized segment channel closed".to_string(),
                                            )
                                        })?;
                                    }
                                    on_segment(&completed)?;
                                    incrementally_finalized_segment_ids.insert(key);
                                }
                            }
                        }
                        // Failures must also unblock the fence so downstream
                        // batches don't deadlock waiting on a segment that
                        // will never publish a Succeeded entry.
                        if let Some(registry) = config.context_registry.as_deref() {
                            for failure in &batch_result.failures {
                                if let Some(segment) = segments_by_id.get(&failure.segment_id.0) {
                                    registry.pre_populate_text(
                                        segment,
                                        String::new(),
                                        SegmentStatus::Failed,
                                    );
                                }
                            }
                        }
                    }
                    all_results.push(batch_result);
                }
                Err(LlmError::InvalidResponse(_))
                    if batch.kind == BatchKind::Translation
                        && request_status == RequestStatus::Truncated
                        && batch.items.len() == 1
                        && batch
                            .items
                            .iter()
                            .filter_map(|item| {
                                compact_retry_attempts_by_item.get(&item.item_id).copied()
                            })
                            .max()
                            .unwrap_or(0)
                            < 3 =>
                {
                    let attempt = batch
                        .items
                        .iter()
                        .filter_map(|item| {
                            compact_retry_attempts_by_item.get(&item.item_id).copied()
                        })
                        .max()
                        .unwrap_or(0)
                        .saturating_add(1);
                    for item in &batch.items {
                        compact_retry_attempts_by_item.insert(item.item_id.clone(), attempt);
                    }
                    set_batch_output_override(
                        &mut escalated_output_tokens_by_item,
                        &batch,
                        max_output_tokens,
                    );
                    progress.emit(bookforge_core::ProgressEvent::Warning {
                        kind: "single_item_batch_compact_retry".to_string(),
                        message: format!(
                            "single-item batch {} exhausted max_output_tokens {}; compact anti-repetition retry {attempt}/3",
                            batch.id, max_output_tokens,
                        ),
                        timestamp_ms: bookforge_core::progress::now_ms(),
                    });
                    pending_queue.push_back(batch);
                }
                Err(LlmError::InvalidResponse(_))
                    if batch.kind == BatchKind::Translation
                        && request_status == RequestStatus::Truncated
                        && !output_escalated
                        && next_max_output_tokens.is_some() =>
                {
                    let next_max_output_tokens =
                        next_max_output_tokens.expect("checked Some above");
                    progress.emit(bookforge_core::ProgressEvent::Warning {
                        kind: "batch_truncation_escalated_retry".to_string(),
                        message: format!(
                            "batch {} exhausted max_output_tokens {}; retrying once with {} before splitting",
                            batch.id, max_output_tokens, next_max_output_tokens
                        ),
                        timestamp_ms: bookforge_core::progress::now_ms(),
                    });
                    set_batch_output_override(
                        &mut escalated_output_tokens_by_item,
                        &batch,
                        next_max_output_tokens,
                    );
                    pending_queue.push_back(batch);
                }
                Err(error @ LlmError::InvalidResponse(_)) if batch.kind == BatchKind::Repair => {
                    if !is_incomplete_response_error(&error)
                        && request_status != RequestStatus::Truncated
                    {
                        truncation_alert.observe_resolved();
                    }
                    progress.emit(bookforge_core::ProgressEvent::Warning {
                        kind: "repair_batch_invalid_response".to_string(),
                        message: format!(
                            "repair batch {} failed; marking {} items NeedsReview",
                            batch.id,
                            batch.items.len()
                        ),
                        timestamp_ms: bookforge_core::progress::now_ms(),
                    });
                    // Repair batches don't participate in the fence — but
                    // their underlying segments may not be otherwise
                    // resolved, so still unblock anyone waiting on them.
                    unblock_fence_for_batch_failure(
                        config.context_registry.as_deref(),
                        &segments_by_id,
                        &batch.items,
                    );
                    all_results.push(BatchTranslationResult {
                        batch_id: batch.id.clone(),
                        translations: Vec::new(),
                        failures: batch
                            .items
                            .iter()
                            .map(|item| BatchItemFailure {
                                item_id: item.item_id.clone(),
                                segment_id: item.segment_id.clone(),
                                error: "repair batch invalid response".to_string(),
                                input_tokens: None,
                                input_cached_tokens: None,
                                output_tokens: None,
                                tokens_estimated: false,
                            })
                            .collect(),
                        input_tokens: None,
                        input_cached_tokens: None,
                        output_tokens: None,
                    });
                }
                Err(error @ LlmError::InvalidResponse(_))
                    if request_status == RequestStatus::Truncated && batch.items.len() == 1 =>
                {
                    if let Some(sizer) =
                        adaptive_sizer_mut(&mut runtime_sizer, batch_sizer.as_deref_mut())
                    {
                        sizer.on_truncation_for_mode(batch.mode);
                    }
                    truncation_alert.observe_unresolved(&progress);
                    progress.emit(bookforge_core::ProgressEvent::Warning {
                        kind: "single_item_batch_truncated".to_string(),
                        message: format!(
                            "single-item batch {} still exhausted max_output_tokens {}; not splitting further",
                            batch.id, max_output_tokens
                        ),
                        timestamp_ms: bookforge_core::progress::now_ms(),
                    });
                    unblock_fence_for_batch_failure(
                        config.context_registry.as_deref(),
                        &segments_by_id,
                        &batch.items,
                    );
                    all_results.push(BatchTranslationResult {
                        batch_id: batch.id.clone(),
                        translations: Vec::new(),
                        failures: batch
                            .items
                            .iter()
                            .map(|item| BatchItemFailure {
                                item_id: item.item_id.clone(),
                                segment_id: item.segment_id.clone(),
                                error: format!("single-item batch truncated: {error}"),
                                input_tokens: None,
                                input_cached_tokens: None,
                                output_tokens: None,
                                tokens_estimated: false,
                            })
                            .collect(),
                        input_tokens: None,
                        input_cached_tokens: None,
                        output_tokens: None,
                    });
                }
                Err(error @ LlmError::InvalidResponse(_)) if batch.items.len() == 1 => {
                    if !is_incomplete_response_error(&error) {
                        truncation_alert.observe_resolved();
                    }
                    let attempts =
                        increment_batch_item_attempts(&mut single_invalid_attempts_by_item, &batch);
                    let compact_retry_limit =
                        if bookforge_core::style::built_in_sizing_policy_for_target(
                            &config.target_language,
                        )
                        .is_some()
                        {
                            4
                        } else {
                            config.scheduler.max_attempts.max(1)
                        };
                    if attempts < compact_retry_limit {
                        for item in &batch.items {
                            compact_retry_attempts_by_item
                                .insert(item.item_id.clone(), attempts.min(3));
                        }
                        set_batch_output_override(
                            &mut escalated_output_tokens_by_item,
                            &batch,
                            max_output_tokens,
                        );
                        progress.emit(bookforge_core::ProgressEvent::Warning {
                            kind: "single_item_batch_invalid_response_retry".to_string(),
                            message: format!(
                                "single-item batch {} returned invalid response on attempt {}; retrying: {error}",
                                batch.id, attempts
                            ),
                            timestamp_ms: bookforge_core::progress::now_ms(),
                        });
                        pending_queue.push_back(batch);
                    } else {
                        progress.emit(bookforge_core::ProgressEvent::Warning {
                            kind: "single_item_batch_invalid_response".to_string(),
                            message: format!(
                                "single-item batch {} failed after {} attempts; not splitting further",
                                batch.id, attempts
                            ),
                            timestamp_ms: bookforge_core::progress::now_ms(),
                        });
                        unblock_fence_for_batch_failure(
                            config.context_registry.as_deref(),
                            &segments_by_id,
                            &batch.items,
                        );
                        all_results.push(BatchTranslationResult {
                            batch_id: batch.id.clone(),
                            translations: Vec::new(),
                            failures: batch
                                .items
                                .iter()
                                .map(|item| BatchItemFailure {
                                    item_id: item.item_id.clone(),
                                    segment_id: item.segment_id.clone(),
                                    error: format!("single-item batch invalid response: {error}"),
                                    input_tokens: None,
                                    input_cached_tokens: None,
                                    output_tokens: None,
                                    tokens_estimated: false,
                                })
                                .collect(),
                            input_tokens: None,
                            input_cached_tokens: None,
                            output_tokens: None,
                        });
                    }
                }
                Err(error @ LlmError::InvalidResponse(_)) if batch.items.len() > 1 => {
                    let incomplete = is_incomplete_response_error(&error);
                    if let Some(sizer) =
                        adaptive_sizer_mut(&mut runtime_sizer, batch_sizer.as_deref_mut())
                    {
                        if request_status == RequestStatus::Truncated {
                            sizer.on_truncation_for_mode(batch.mode);
                        } else {
                            sizer.on_invalid_json_for_mode(batch.mode);
                        }
                    }
                    if request_status == RequestStatus::Truncated {
                        truncation_alert.observe_unresolved(&progress);
                    } else if !incomplete {
                        truncation_alert.observe_resolved();
                    }
                    let split = split_batch_with_config(&batch, Some(config.as_ref()));
                    if split.len() == 2 {
                        progress.emit(bookforge_core::ProgressEvent::BatchSplit {
                            batch_id: batch.id.clone(),
                            left_items: split[0].items.len(),
                            right_items: split[1].items.len(),
                            timestamp_ms: bookforge_core::progress::now_ms(),
                        });
                    }
                    progress.emit(bookforge_core::ProgressEvent::Warning {
                        kind: if request_status == RequestStatus::Truncated {
                            "batch_truncated_split"
                        } else if incomplete {
                            "batch_incomplete_response_split"
                        } else {
                            "batch_invalid_response_split"
                        }
                        .to_string(),
                        message: format!(
                            "batch {} failed with {}, splitting",
                            batch.id,
                            if request_status == RequestStatus::Truncated {
                                "truncated output"
                            } else if incomplete {
                                "incomplete output"
                            } else {
                                "invalid response"
                            }
                        ),
                        timestamp_ms: bookforge_core::progress::now_ms(),
                    });
                    pending_queue.extend(split);
                }
                Err(ref error) if is_transient(error) && batch.kind == BatchKind::Translation => {
                    truncation_alert.observe_resolved();
                    let attempts =
                        increment_batch_item_attempts(&mut transient_attempts_by_item, &batch);
                    if attempts < config.scheduler.max_attempts.max(1) {
                        progress.emit(bookforge_core::ProgressEvent::Warning {
                            kind: "batch_transient_retry".to_string(),
                            message: format!(
                                "batch {} transient error on attempt {}; retrying: {error}",
                                batch.id, attempts
                            ),
                            timestamp_ms: bookforge_core::progress::now_ms(),
                        });
                        pending_queue.push_back(batch);
                    } else {
                        progress.emit(bookforge_core::ProgressEvent::Warning {
                            kind: "batch_transient_exhausted".to_string(),
                            message: format!(
                                "batch {} failed after {} transient attempts: {error}",
                                batch.id, attempts
                            ),
                            timestamp_ms: bookforge_core::progress::now_ms(),
                        });
                        unblock_fence_for_batch_failure(
                            config.context_registry.as_deref(),
                            &segments_by_id,
                            &batch.items,
                        );
                        all_results.push(BatchTranslationResult {
                            batch_id: batch.id.clone(),
                            translations: Vec::new(),
                            failures: batch
                                .items
                                .iter()
                                .map(|item| BatchItemFailure {
                                    item_id: item.item_id.clone(),
                                    segment_id: item.segment_id.clone(),
                                    error: format!("{error}"),
                                    input_tokens: None,
                                    input_cached_tokens: None,
                                    output_tokens: None,
                                    tokens_estimated: false,
                                })
                                .collect(),
                            input_tokens: None,
                            input_cached_tokens: None,
                            output_tokens: None,
                        });
                    }
                }
                Err(error) => {
                    truncation_alert.observe_resolved();
                    progress.emit(bookforge_core::ProgressEvent::Warning {
                        kind: "batch_failed".to_string(),
                        message: format!("batch {} failed: {error}", batch.id),
                        timestamp_ms: bookforge_core::progress::now_ms(),
                    });
                    unblock_fence_for_batch_failure(
                        config.context_registry.as_deref(),
                        &segments_by_id,
                        &batch.items,
                    );
                    all_results.push(BatchTranslationResult {
                        batch_id: batch.id.clone(),
                        translations: Vec::new(),
                        failures: batch
                            .items
                            .iter()
                            .map(|item| BatchItemFailure {
                                item_id: item.item_id.clone(),
                                segment_id: item.segment_id.clone(),
                                error: format!("{error}"),
                                input_tokens: None,
                                input_cached_tokens: None,
                                output_tokens: None,
                                tokens_estimated: false,
                            })
                            .collect(),
                        input_tokens: None,
                        input_cached_tokens: None,
                        output_tokens: None,
                    });
                }
            }
        }
        if stop_dispatch {
            break;
        }
        pending = pending_queue.into();
    }

    progress.emit(bookforge_core::ProgressEvent::Warning {
        kind: "batch_finalize_started".to_string(),
        message: format!(
            "batch provider requests complete; aggregating {} batch results",
            all_results.len()
        ),
        timestamp_ms: bookforge_core::progress::now_ms(),
    });

    let mut segment_translations: HashMap<String, SegmentTranslation> = HashMap::new();

    let segments_by_id: HashMap<&str, &Segment> =
        segments.iter().map(|s| (s.id.0.as_str(), s)).collect();

    let make_entry = |seg_id: &str,
                      status: SegmentStatus,
                      error: Option<String>,
                      input_tokens: Option<u64>,
                      input_cached_tokens: Option<u64>,
                      output_tokens: Option<u64>,
                      tokens_estimated: bool|
     -> SegmentTranslation {
        if let Some(seg) = segments_by_id.get(seg_id) {
            SegmentTranslation {
                segment_id: SegmentId(seg_id.to_string()),
                ordinal: seg.ordinal,
                block_ids: seg.block_ids.clone(),
                blocks: Vec::new(),
                checksum: seg.checksum.clone(),
                status,
                template: "batch".to_string(),
                error,
                input_tokens,
                input_cached_tokens,
                output_tokens,
                tokens_estimated,
            }
        } else {
            SegmentTranslation {
                segment_id: SegmentId(seg_id.to_string()),
                ordinal: 0,
                block_ids: Vec::new(),
                blocks: Vec::new(),
                checksum: String::new(),
                status,
                template: "batch".to_string(),
                error,
                input_tokens,
                input_cached_tokens,
                output_tokens,
                tokens_estimated,
            }
        }
    };

    for batch_result in &all_results {
        for translation in &batch_result.translations {
            let seg_id = translation.segment_id.0.clone();
            let entry = segment_translations
                .entry(seg_id.clone())
                .or_insert_with(|| {
                    make_entry(
                        &seg_id,
                        SegmentStatus::Succeeded,
                        None,
                        None,
                        None,
                        None,
                        false,
                    )
                });
            add_usage(entry, translation);
            if let Some(warning) = &translation.warning {
                append_translation_error(entry, warning);
            }
            if let Some(source_item) = all_items.get(&translation.item_id) {
                entry.blocks.push(BlockTranslation {
                    block_id: source_item.block_id.clone(),
                    text: translation.text.clone(),
                });
            } else {
                progress.emit(bookforge_core::ProgressEvent::Warning {
                    kind: "batch_internal_missing_item".to_string(),
                    message: format!(
                        "batch translation item_id {} missing from all_items; skipping (internal state bug)",
                        translation.item_id
                    ),
                    timestamp_ms: bookforge_core::progress::now_ms(),
                });
            }
        }

        for failure in &batch_result.failures {
            let seg_id = failure.segment_id.0.clone();
            let entry = match segment_translations.entry(seg_id.clone()) {
                Entry::Occupied(entry) => {
                    let entry = entry.into_mut();
                    entry.status = SegmentStatus::NeedsReview;
                    append_translation_error(entry, &failure.error);
                    entry
                }
                Entry::Vacant(entry) => entry.insert(make_entry(
                    &seg_id,
                    SegmentStatus::NeedsReview,
                    Some(failure.error.clone()),
                    None,
                    None,
                    None,
                    false,
                )),
            };
            add_failure_usage(entry, failure);
        }
    }

    let repair_items: Vec<(BatchItemFailure, TranslationBatchItem)> = all_results
        .iter()
        .flat_map(|r| &r.failures)
        .filter(|f| f.segment_id.0 != "unknown")
        .filter(|f| repairable_batch_failure(f))
        .filter_map(|f| {
            all_items
                .get(f.item_id.as_str())
                .map(|item| (f.clone(), (*item).clone()))
        })
        .collect();

    progress.emit(bookforge_core::ProgressEvent::Warning {
        kind: "batch_aggregation_finished".to_string(),
        message: format!(
            "batch aggregation produced {} segment records and {} repair candidates",
            segment_translations.len(),
            repair_items.len()
        ),
        timestamp_ms: bookforge_core::progress::now_ms(),
    });

    if !repair_items.is_empty() {
        progress.emit(bookforge_core::ProgressEvent::BatchRepairStarted {
            failed_item_count: repair_items.len(),
            timestamp_ms: bookforge_core::progress::now_ms(),
        });

        let repair_errors: Arc<HashMap<String, String>> = Arc::new(
            repair_items
                .iter()
                .map(|(failure, _)| (failure.item_id.clone(), failure.error.clone()))
                .collect(),
        );
        let repair_batch_item_limit = repair_batch_item_limit(&config.target_language);
        let mut repair_batches: VecDeque<TranslationBatch> = repair_items
            .iter()
            .map(|(_, item)| item.clone())
            .collect::<Vec<_>>()
            .chunks(repair_batch_item_limit)
            .enumerate()
            .map(|(idx, items)| {
                let items = items.to_vec();
                TranslationBatch {
                    id: format!("repair_{idx:04}"),
                    ordinal: idx,
                    mode: BatchMode::Plain,
                    kind: BatchKind::Repair,
                    token_estimate: items
                        .iter()
                        .map(|item| token_estimate(&item.source_text))
                        .sum(),
                    items,
                    // Repair batches don't participate in the sliding-
                    // context fence (they fix JSON syntax, not translation
                    // content); the sentinel section_id is harmless.
                    section_id: bookforge_core::ir::SectionId(String::new()),
                }
            })
            .collect();

        let mut repaired_count = 0usize;
        let mut repair_attempts_by_batch: HashMap<String, usize> = HashMap::new();
        let mut repair_tasks = JoinSet::<RepairWorkerOutput>::new();

        while !repair_batches.is_empty() || !repair_tasks.is_empty() {
            loop {
                let runtime_snapshot = config
                    .runtime_settings
                    .as_ref()
                    .map(|receiver| receiver.borrow().clone());
                let concurrency = runtime_snapshot
                    .as_ref()
                    .map(|runtime| runtime.concurrency)
                    .unwrap_or(config.scheduler.concurrency)
                    .max(1);
                if repair_tasks.len() >= concurrency {
                    break;
                }
                let Some(repair_batch) = repair_batches.pop_front() else {
                    break;
                };
                progress.emit(bookforge_core::ProgressEvent::BatchQueued {
                    batch_id: repair_batch.id.clone(),
                    item_count: repair_batch.items.len(),
                    timestamp_ms: bookforge_core::progress::now_ms(),
                });

                let provider = provider.clone();
                let library = library.clone();
                let mut task_config = config.as_ref().clone();
                if let Some(runtime) = runtime_snapshot {
                    task_config.scheduler.concurrency = runtime.concurrency.max(1);
                    task_config.batch_max_output_tokens = runtime.batch_max_output_tokens;
                    task_config.runtime_settings = Some(runtime.frozen_receiver());
                }
                let config = Arc::new(task_config);
                let repair_errors = repair_errors.clone();
                let progress = progress.clone();
                let section_titles = section_titles.clone();
                let active_requests = active_requests.clone();

                repair_tasks.spawn(async move {
                    let started = std::time::Instant::now();
                    let is_reasoning = provider.is_reasoning();
                    let max_output_tokens =
                        capped_batch_max_output_tokens(&repair_batch, &config, is_reasoning);
                    let request_id = format!("batch_{}", repair_batch.id);
                    let (runtime_config_revision, provider_max_attempts) =
                        config.request_runtime_metadata();
                    let (_in_flight, active_request_count) =
                        InFlightRequestGuard::start(active_requests);

                    progress.emit(bookforge_core::ProgressEvent::RequestStarted {
                        request_id: request_id.clone(),
                        batch_id: Some(repair_batch.id.clone()),
                        segment_id: None,
                        provider: Some(config.provider.clone()),
                        model: Some(config.model.clone()),
                        prompt_template: Some("batch_repair".to_string()),
                        items: repair_batch.items.len(),
                        estimated_input_tokens: repair_batch.token_estimate,
                        max_output_tokens: Some(max_output_tokens),
                        active_requests: active_request_count,
                        target_concurrency: config.scheduler.concurrency,
                        runtime_config_revision,
                        provider_max_attempts,
                        timestamp_ms: bookforge_core::progress::now_ms(),
                    });

                    let items_json: Vec<serde_json::Value> = repair_batch
                        .items
                        .iter()
                        .map(|item| {
                            let protected = item
                                .protected_spans
                                .iter()
                                .map(|span| span.text.as_str())
                                .collect::<Vec<_>>();
                            serde_json::json!({
                                "id": item.item_id,
                                "source_text": item.source_text,
                                "required_markers": item.required_markers,
                                "protected": protected,
                            })
                        })
                        .collect();

                    let errors_json: Vec<serde_json::Value> = repair_batch
                        .items
                        .iter()
                        .map(|item| {
                            serde_json::json!({
                                "id": item.item_id,
                                "error": repair_errors
                                    .get(&item.item_id)
                                    .map(|error| provider_safe_validation_error(error))
                                    .unwrap_or_else(|| "invalid batch item".to_string()),
                            })
                        })
                        .collect();
                    let guidance_json: Vec<serde_json::Value> = repair_batch
                        .items
                        .iter()
                        .filter_map(|item| {
                            config
                                .glossary
                                .guidance_by_segment
                                .get(&item.segment_id.0)
                                .map(|guidance| {
                                    serde_json::json!({
                                        "id": item.item_id,
                                        "guidance": guidance,
                                    })
                                })
                        })
                        .collect();

                    let mut vars = Substitutions::new();
                    vars.raw(
                        "items_json",
                        serde_json::to_string(&items_json).unwrap_or_default(),
                    )
                    .raw(
                        "errors_json",
                        serde_json::to_string(&errors_json).unwrap_or_default(),
                    )
                    .raw(
                        "guidance_json",
                        serde_json::to_string(&guidance_json).unwrap_or_default(),
                    )
                    .raw(
                        "source_language",
                        config
                            .source_language
                            .as_deref()
                            .unwrap_or("source language"),
                    )
                    .raw("target_language", &config.target_language)
                    .raw(
                        "style_block",
                        config
                            .style
                            .as_ref()
                            .map(|style| style.rendered_block.as_str())
                            .unwrap_or(""),
                    );

                    let repair_template = if config.compact_prompts {
                        &library.batch_repair_compact
                    } else {
                        &library.batch_repair
                    };

                    let mut finish_reason = None;
                    let mut returned_items = None;
                    let result = match repair_template.render(&vars) {
                        Ok(rendered) => {
                            match provider
                                .complete(CompletionRequest {
                                    system: rendered.system,
                                    user: rendered.user,
                                    response_format: ResponseFormat::Json,
                                    temperature: 0.1,
                                    max_output_tokens: Some(max_output_tokens),
                                    metadata: RequestMetadata {
                                        segment_id: Some(format!("batch_{}", repair_batch.id)),
                                        block_ids: repair_batch
                                            .items
                                            .iter()
                                            .map(|item| item.block_id.0.clone())
                                            .collect(),
                                        prompt_template: Some(repair_template.name.clone()),
                                        prompt_version: Some(repair_template.version.clone()),
                                        provider: Some(config.provider.clone()),
                                        model: Some(config.model.clone()),
                                        source_checksum: None,
                                        runtime_config_revision,
                                        provider_max_attempts,
                                    },
                                })
                                .await
                            {
                                Ok(response) => {
                                    finish_reason = Some(response.finish_reason);
                                    returned_items =
                                        batch_response_item_count(&repair_batch, &response.content);
                                    if response.finish_reason == FinishReason::Length {
                                        Err(LlmError::InvalidResponse(
                                            "repair batch output was truncated: max_output_tokens limit reached"
                                                .to_string(),
                                        ))
                                    } else {
                                        match parse_batch_response_with_validation(
                                            &repair_batch,
                                            &response.content,
                                            validate_source_copy,
                                            Some(&section_titles),
                                            (!config.provider.eq_ignore_ascii_case("mock"))
                                                .then_some(config.target_language.as_str()),
                                        ) {
                                            Ok(mut repaired) => {
                                                repaired.input_tokens = response.input_tokens;
                                                repaired.input_cached_tokens =
                                                    response.input_cached_tokens;
                                                repaired.output_tokens = response.output_tokens;
                                                Ok(repaired)
                                            }
                                            Err(error) => Err(LlmError::InvalidResponse(error)),
                                        }
                                    }
                                }
                                Err(error) => Err(error),
                            }
                        }
                        Err(error) => Err(LlmError::Provider(format!(
                            "failed to render repair prompt: {error}"
                        ))),
                    };

                    RepairWorkerOutput {
                        batch: repair_batch,
                        result,
                        finish_reason,
                        returned_items,
                        latency_ms: started.elapsed().as_millis() as u64,
                        max_output_tokens,
                    }
                });
            }

            let Some(joined) = repair_tasks.join_next().await else {
                continue;
            };
            let RepairWorkerOutput {
                batch,
                result,
                finish_reason,
                returned_items,
                latency_ms,
                max_output_tokens,
            } = joined
                .map_err(|err| LlmError::Provider(format!("repair worker task failed: {err}")))?;

            let recorded_status =
                batch_request_metric_status(&result, batch.items.len(), returned_items);
            let recorded_finish_reason =
                finish_reason.map(|reason| finish_reason_label(reason).to_string());
            let repair_retry_count = repair_attempts_by_batch
                .get(&batch.id)
                .copied()
                .unwrap_or(0);

            progress.emit(bookforge_core::ProgressEvent::RequestFinished {
                request_id: format!("batch_{}", batch.id),
                batch_id: Some(batch.id.clone()),
                segment_id: None,
                status: recorded_status.clone(),
                latency_ms,
                status_code: None,
                finish_reason: recorded_finish_reason.clone(),
                retry_count: repair_retry_count,
                input_tokens: result.as_ref().ok().and_then(|r| r.input_tokens),
                output_tokens: result.as_ref().ok().and_then(|r| r.output_tokens),
                error_kind: result.as_ref().err().map(|e| format!("{e:?}")),
                timestamp_ms: bookforge_core::progress::now_ms(),
            });

            telemetry.record(ProviderRequestMetric {
                request_id: format!("batch_{}", batch.id),
                batch_id: Some(batch.id.clone()),
                provider: config.provider.clone(),
                model: config.model.clone(),
                profile: config.profile.namespace_str().to_string(),
                items: batch.items.len(),
                estimated_input_tokens: batch.token_estimate,
                max_output_tokens: Some(max_output_tokens),
                input_tokens: result.as_ref().ok().and_then(|r| r.input_tokens),
                output_tokens: result.as_ref().ok().and_then(|r| r.output_tokens),
                latency_ms,
                finish_reason: recorded_finish_reason,
                status: recorded_status,
                status_code: None,
                retry_count: repair_retry_count,
                backoff_ms: 0,
                error_kind: None,
            });

            match result {
                Ok(repaired) => {
                    if !repaired.failures.is_empty() {
                        progress.emit(bookforge_core::ProgressEvent::Warning {
                            kind: "repair_batch_item_failures".to_string(),
                            message: format!(
                                "repair batch {} returned {} present-but-invalid items; keeping them NeedsReview",
                                batch.id,
                                repaired.failures.len()
                            ),
                            timestamp_ms: bookforge_core::progress::now_ms(),
                        });
                    }
                    for translation in repaired.translations {
                        let Some(source_item) = all_items.get(&translation.item_id) else {
                            continue;
                        };
                        let mut completed_segment = None;
                        if let Some(existing) =
                            segment_translations.get_mut(&translation.segment_id.0)
                        {
                            existing.status = SegmentStatus::Succeeded;
                            let retained_warnings =
                                existing.error.as_deref().and_then(warning_findings_only);
                            existing.error = retained_warnings;
                            if let Some(warning) = &translation.warning {
                                append_translation_error(existing, warning);
                            }
                            if let Some(block) = existing
                                .blocks
                                .iter_mut()
                                .find(|b| b.block_id == source_item.block_id)
                            {
                                block.text = translation.text;
                            } else {
                                existing.blocks.push(BlockTranslation {
                                    block_id: source_item.block_id.clone(),
                                    text: translation.text,
                                });
                            }
                            repaired_count += 1;

                            if !incrementally_finalized_segment_ids
                                .contains(&translation.segment_id.0)
                            {
                                let mut candidate = existing.clone();
                                let (blocks, missing, extra, duplicate) = order_blocks_by_segment(
                                    &candidate.block_ids,
                                    std::mem::take(&mut candidate.blocks),
                                );
                                if missing.is_empty() && extra.is_empty() && duplicate.is_empty() {
                                    candidate.blocks = blocks;
                                    completed_segment = Some(candidate);
                                }
                            }
                        }
                        if let Some(completed) = completed_segment {
                            if let Some(ref tx) = finalized_tx {
                                tx.send(completed.clone()).await.map_err(|_| {
                                    LlmError::Provider(
                                        "finalized segment channel closed".to_string(),
                                    )
                                })?;
                            }
                            on_segment(&completed)?;
                            incrementally_finalized_segment_ids
                                .insert(completed.segment_id.0.clone());
                        }
                    }
                }
                Err(error @ LlmError::InvalidResponse(_)) if repair_retry_count < 1 => {
                    repair_attempts_by_batch.insert(batch.id.clone(), repair_retry_count + 1);
                    progress.emit(bookforge_core::ProgressEvent::Warning {
                        kind: "repair_batch_invalid_response_retry".to_string(),
                        message: format!(
                            "repair batch {} returned an invalid or incomplete response; retrying once for {} items: {error}",
                            batch.id,
                            batch.items.len()
                        ),
                        timestamp_ms: bookforge_core::progress::now_ms(),
                    });
                    repair_batches.push_back(batch);
                }
                Err(error) => {
                    progress.emit(bookforge_core::ProgressEvent::Warning {
                        kind: "repair_batch_failed".to_string(),
                        message: format!(
                            "repair batch {} failed for {} items: {error}",
                            batch.id,
                            batch.items.len()
                        ),
                        timestamp_ms: bookforge_core::progress::now_ms(),
                    });
                }
            }
        }

        progress.emit(bookforge_core::ProgressEvent::BatchRepairFinished {
            repaired_items: repaired_count,
            still_failed_items: repair_items.len().saturating_sub(repaired_count),
            timestamp_ms: bookforge_core::progress::now_ms(),
        });
    }

    let mut translations: Vec<SegmentTranslation> = segment_translations.into_values().collect();

    for translation in &mut translations {
        let (ordered_blocks, missing, extra, duplicate) = order_blocks_by_segment(
            &translation.block_ids,
            std::mem::take(&mut translation.blocks),
        );
        translation.blocks = ordered_blocks;

        if (!missing.is_empty() || !extra.is_empty() || !duplicate.is_empty())
            && (translation.status == SegmentStatus::Succeeded || !translation.blocks.is_empty())
        {
            translation.status = SegmentStatus::NeedsReview;
            let error = format!(
                "batch translation block mismatch: missing={missing:?}, extra={extra:?}, duplicate={duplicate:?}",
            );
            append_translation_error(translation, &error);
        }
    }

    progress.emit(bookforge_core::ProgressEvent::Warning {
        kind: "batch_finalized_segments".to_string(),
        message: format!(
            "batch finalization produced {} segment translations",
            translations.len()
        ),
        timestamp_ms: bookforge_core::progress::now_ms(),
    });

    for translation in &mut translations {
        if incrementally_finalized_segment_ids.contains(&translation.segment_id.0) {
            continue;
        }
        if let Some(ref tx) = finalized_tx {
            tx.send(translation.clone())
                .await
                .map_err(|_| LlmError::Provider("finalized segment channel closed".to_string()))?;
        }
        on_segment(translation)?;
    }

    Ok(translations)
}

async fn wait_for_batch_resume_or_stop<C>(
    signal: &PauseSignal,
    on_control_boundary: &mut C,
) -> Result<PauseState, LlmError>
where
    C: FnMut(&PauseSignal) -> Result<(), LlmError>,
{
    loop {
        on_control_boundary(signal)?;
        match signal.state() {
            PauseState::Running => return Ok(PauseState::Running),
            PauseState::Stopped => return Ok(PauseState::Stopped),
            PauseState::Paused => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

pub(super) struct BatchTranslationRequest<'a> {
    pub config: &'a TranslationRunConfig,
    pub max_output_tokens_override: Option<u32>,
    pub context_pairs: Vec<crate::scheduler::CompletedContext>,
    pub validate_source_copy: bool,
    pub section_titles: &'a HashMap<String, String>,
    pub compact_retry_attempt: usize,
}

#[cfg(test)]
pub(super) async fn translate_one_batch(
    provider: Arc<impl LlmProvider>,
    library: Arc<PromptLibrary>,
    batch: TranslationBatch,
    request: BatchTranslationRequest<'_>,
) -> Result<BatchTranslationResult, LlmError> {
    translate_one_batch_with_evidence(provider, library, batch, request)
        .await
        .result
}

async fn translate_one_batch_with_evidence(
    provider: Arc<impl LlmProvider>,
    library: Arc<PromptLibrary>,
    batch: TranslationBatch,
    request: BatchTranslationRequest<'_>,
) -> BatchRequestOutput {
    let BatchTranslationRequest {
        config,
        max_output_tokens_override,
        context_pairs,
        validate_source_copy,
        section_titles,
        compact_retry_attempt,
    } = request;
    let context_block = crate::scheduler::render_context_pairs(&context_pairs);
    let items_json = render_batch_items(&batch, config);
    let template = if config.compact_prompts {
        match batch.mode {
            BatchMode::Plain | BatchMode::TurboTextOnly => &library.batch_plain_compact,
            BatchMode::MarkerSafe => &library.batch_marker_safe_compact,
            BatchMode::RunPreserving => &library.batch_run_preserving_compact,
        }
    } else {
        match batch.mode {
            BatchMode::Plain | BatchMode::TurboTextOnly => &library.batch_plain,
            BatchMode::MarkerSafe => &library.batch_marker_safe,
            BatchMode::RunPreserving => &library.batch_run_preserving,
        }
    };

    let mut vars = Substitutions::new();
    vars.string(
        "source_language",
        config
            .source_language
            .as_deref()
            .unwrap_or("the source language"),
    )
    .string("target_language", &config.target_language)
    .raw(
        "style_guide_block",
        config
            .style
            .as_ref()
            .map(|s| s.rendered_block.clone())
            .unwrap_or_default(),
    )
    .raw(
        "entity_agreement_block",
        config
            .entities
            .as_ref()
            .map(|e| e.rendered_block.clone())
            .unwrap_or_default(),
    )
    .raw("context_translation_pairs", context_block)
    .raw(
        "prompt_extra",
        config.glossary.prompt_extra.clone().unwrap_or_default(),
    )
    .raw("items_json", items_json);

    let mut rendered = match template.render(&vars) {
        Ok(rendered) => rendered,
        Err(error) => {
            return BatchRequestOutput {
                result: Err(LlmError::Provider(error.to_string())),
                finish_reason: None,
                returned_items: None,
            };
        }
    };
    if compact_retry_attempt > 0 {
        rendered.user.push_str(&format!(
            "\n\nRECOVERY MODE {compact_retry_attempt}: Return one compact JSON object only. Translate every item exactly once. Do not repeat any word, sentence, item, or explanation. End immediately after the closing brace."
        ));
    }

    let max_tokens = max_output_tokens_override
        .unwrap_or_else(|| capped_batch_max_output_tokens(&batch, config, provider.is_reasoning()));
    let (runtime_config_revision, provider_max_attempts) = config.request_runtime_metadata();

    let response = provider
        .complete(CompletionRequest {
            system: rendered.system,
            user: rendered.user,
            response_format: ResponseFormat::Json,
            temperature: match compact_retry_attempt {
                0 => 0.2,
                1 => 0.0,
                2 => 0.4,
                _ => 0.7,
            },
            max_output_tokens: Some(max_tokens),
            metadata: RequestMetadata {
                segment_id: Some(format!("batch_{}", batch.id)),
                block_ids: batch.items.iter().map(|i| i.block_id.0.clone()).collect(),
                prompt_template: Some(template.name.clone()),
                prompt_version: Some(template.version.clone()),
                provider: Some(config.provider.clone()),
                model: Some(config.model.clone()),
                source_checksum: None,
                runtime_config_revision,
                provider_max_attempts,
            },
        })
        .await;

    match response {
        Ok(resp) => {
            let finish_reason = Some(resp.finish_reason);
            let returned_items = batch_response_item_count(&batch, &resp.content);
            if resp.finish_reason == FinishReason::Length {
                return BatchRequestOutput {
                    result: Err(LlmError::InvalidResponse(
                        "batch output was truncated: max_output_tokens limit reached".to_string(),
                    )),
                    finish_reason,
                    returned_items,
                };
            }

            let result = parse_batch_response_with_validation(
                &batch,
                &resp.content,
                validate_source_copy,
                Some(section_titles),
                (!config.provider.eq_ignore_ascii_case("mock"))
                    .then_some(config.target_language.as_str()),
            )
            .map_err(LlmError::InvalidResponse)
            .map(|mut result| {
                result.input_tokens = resp.input_tokens;
                result.input_cached_tokens = resp.input_cached_tokens;
                result.output_tokens = resp.output_tokens;
                apportion_batch_usage(&batch, &mut result);
                result
            });
            BatchRequestOutput {
                result,
                finish_reason,
                returned_items,
            }
        }
        Err(error) => BatchRequestOutput {
            result: Err(error),
            finish_reason: None,
            returned_items: None,
        },
    }
}

async fn context_pairs_for_batch(
    batch: &TranslationBatch,
    config: &TranslationRunConfig,
) -> Vec<crate::scheduler::CompletedContext> {
    // Sliding-context fence (ROADMAP §6.4 — PR5 makes this work in batch
    // mode). build_translation_batches guarantees no batch crosses a
    // section boundary, so awaiting the batch's earliest segment is safe:
    // its prior-N dependencies are necessarily in *earlier* batches of
    // the same section (or earlier sections, depending on scope) and
    // can't deadlock on a sibling item in this same batch. In strict mode
    // the scheduler calls this before acquiring request concurrency so
    // context waiters cannot starve prerequisite split batches; in
    // best-effort mode it is called after permits are held and returns
    // without waiting.
    match (config.context_registry.as_deref(), batch.kind) {
        (Some(registry), BatchKind::Translation) if config.context.enabled() => {
            let earliest = batch
                .items
                .iter()
                .min_by_key(|item| item.ordinal)
                .map(|item| item.segment_id.clone());
            match earliest {
                Some(segment_id) => {
                    registry
                        .await_context_for(&segment_id, config.context)
                        .await
                }
                None => Vec::new(),
            }
        }
        _ => Vec::new(),
    }
}

fn apportion_batch_usage(batch: &TranslationBatch, result: &mut BatchTranslationResult) {
    let total_input = result.input_tokens;
    let total_cached = result.input_cached_tokens;
    let total_output = result.output_tokens;
    if total_input.is_none() && total_cached.is_none() && total_output.is_none() {
        return;
    }

    let weights = batch
        .items
        .iter()
        .map(|item| token_estimate(&item.source_text).max(1) as u64)
        .collect::<Vec<_>>();
    if weights.is_empty() {
        return;
    }

    let input = apportion(total_input, &weights);
    let cached = apportion(total_cached, &weights);
    let output = apportion(total_output, &weights);
    let estimated = batch.items.len() > 1;

    let usage_by_item = batch
        .items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            (
                item.item_id.as_str(),
                (input[index], cached[index], output[index]),
            )
        })
        .collect::<HashMap<_, _>>();

    for translation in &mut result.translations {
        if let Some((input, cached, output)) = usage_by_item.get(translation.item_id.as_str()) {
            translation.input_tokens = *input;
            translation.input_cached_tokens = *cached;
            translation.output_tokens = *output;
            translation.tokens_estimated = estimated;
        }
    }

    for failure in &mut result.failures {
        if let Some((input, cached, output)) = usage_by_item.get(failure.item_id.as_str()) {
            failure.input_tokens = *input;
            failure.input_cached_tokens = *cached;
            failure.output_tokens = *output;
            failure.tokens_estimated = estimated;
        }
    }
}

fn add_usage(entry: &mut SegmentTranslation, item: &BatchItemTranslation) {
    entry.input_tokens = add_optional(entry.input_tokens, item.input_tokens);
    entry.input_cached_tokens = add_optional(entry.input_cached_tokens, item.input_cached_tokens);
    entry.output_tokens = add_optional(entry.output_tokens, item.output_tokens);
    entry.tokens_estimated |= item.tokens_estimated;
}

fn add_failure_usage(entry: &mut SegmentTranslation, item: &BatchItemFailure) {
    entry.input_tokens = add_optional(entry.input_tokens, item.input_tokens);
    entry.input_cached_tokens = add_optional(entry.input_cached_tokens, item.input_cached_tokens);
    entry.output_tokens = add_optional(entry.output_tokens, item.output_tokens);
    entry.tokens_estimated |= item.tokens_estimated;
}

fn append_translation_error(entry: &mut SegmentTranslation, error: &str) {
    match entry.error.as_mut() {
        Some(existing) if existing == error => {}
        Some(existing) => {
            existing.push_str("; ");
            existing.push_str(error);
        }
        None => entry.error = Some(error.to_string()),
    }
}

fn warning_findings_only(error: &str) -> Option<String> {
    let warnings = error
        .split("; ")
        .filter(|fragment| fragment.starts_with("warning: "))
        .collect::<Vec<_>>()
        .join("; ");
    (!warnings.is_empty()).then_some(warnings)
}

fn provider_safe_validation_error(error: &str) -> String {
    let mut message = error.to_string();
    while let Some(start) = message.find(" [kind=") {
        let Some(relative_end) = message[start..].find(']') else {
            break;
        };
        message.replace_range(start..=start + relative_end, "");
    }
    message
}

fn repairable_batch_failure(failure: &BatchItemFailure) -> bool {
    !matches!(
        failure.error.as_str(),
        error if error.starts_with("HTTP status ")
            || error.starts_with("HTTP error:")
            || error.starts_with("provider error:")
            || error.contains("semaphore closed")
            || error.contains("concurrency limiter closed")
    )
}

fn order_blocks_by_segment(
    block_ids: &[BlockId],
    blocks: Vec<BlockTranslation>,
) -> (Vec<BlockTranslation>, Vec<String>, Vec<String>, Vec<String>) {
    let mut by_id: HashMap<BlockId, Vec<BlockTranslation>> = HashMap::new();
    for block in blocks {
        by_id.entry(block.block_id.clone()).or_default().push(block);
    }

    let mut ordered = Vec::with_capacity(block_ids.len());
    let mut missing = Vec::new();
    let mut duplicate = Vec::new();
    for block_id in block_ids {
        match by_id.remove(block_id) {
            Some(mut matches) => {
                if matches.len() > 1 {
                    duplicate.push(block_id.0.clone());
                }
                ordered.push(matches.remove(0));
            }
            None => missing.push(block_id.0.clone()),
        }
    }

    let mut extra = by_id.keys().map(|id| id.0.clone()).collect::<Vec<_>>();
    extra.sort();
    for mut extras in by_id.into_values() {
        ordered.append(&mut extras);
    }

    missing.sort();
    duplicate.sort();
    (ordered, missing, extra, duplicate)
}

fn add_optional(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.saturating_add(right)),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

fn apportion(total: Option<u64>, weights: &[u64]) -> Vec<Option<u64>> {
    let Some(total) = total else {
        return vec![None; weights.len()];
    };
    let weight_sum = weights.iter().sum::<u64>().max(1);
    let mut values = Vec::with_capacity(weights.len());
    let mut used = 0_u64;
    for (index, weight) in weights.iter().enumerate() {
        let value = if index + 1 == weights.len() {
            total.saturating_sub(used)
        } else {
            total.saturating_mul(*weight) / weight_sum
        };
        used = used.saturating_add(value);
        values.push(Some(value));
    }
    values
}

fn request_status_from_error(error: &LlmError) -> &'static str {
    match error {
        LlmError::HttpStatus { status: 429, .. } => "rate_limited",
        LlmError::HttpStatus { status, .. } if *status >= 500 => "server_error",
        LlmError::Http(e) if e.is_timeout() => "timeout",
        LlmError::Http(e) if e.is_connect() => "connect_error",
        LlmError::InvalidResponse(msg) if msg.contains("truncated") => "truncated",
        LlmError::InvalidResponse(_) => "invalid_response",
        LlmError::Json(_) => "json_error",
        _ => "error",
    }
}

fn batch_request_metric_status<T>(
    result: &Result<T, LlmError>,
    requested_items: usize,
    returned_items: Option<usize>,
) -> String {
    match result {
        Ok(_) => "ok".to_string(),
        Err(error) if is_incomplete_response_error(error) => returned_items.map_or_else(
            || "incomplete".to_string(),
            |returned| format!("incomplete:{returned}/{requested_items}"),
        ),
        Err(error) => request_status_from_error(error).to_string(),
    }
}

fn is_incomplete_response_error(error: &LlmError) -> bool {
    matches!(
        error,
        LlmError::InvalidResponse(message) if message.starts_with("batch response incomplete:")
    )
}

fn finish_reason_label(reason: FinishReason) -> &'static str {
    match reason {
        FinishReason::Stop => "stop",
        FinishReason::Length => "length",
        FinishReason::ContentFilter => "content_filter",
        FinishReason::ToolCalls => "tool_calls",
        FinishReason::Unknown => "unknown",
    }
}

pub(super) fn request_status_for_controller<T>(result: &Result<T, LlmError>) -> RequestStatus {
    match result {
        Ok(_) => RequestStatus::Ok,
        Err(LlmError::HttpStatus { status: 429, .. }) => RequestStatus::RateLimited,
        Err(LlmError::HttpStatus { status, .. }) if *status >= 500 => RequestStatus::ServerError,
        Err(LlmError::Http(error)) if error.is_timeout() => RequestStatus::Timeout,
        Err(LlmError::Http(error)) if error.is_connect() => RequestStatus::ConnectError,
        Err(LlmError::InvalidResponse(message)) if message.contains("truncated") => {
            RequestStatus::Truncated
        }
        Err(LlmError::InvalidResponse(_)) | Err(LlmError::Json(_)) => RequestStatus::InvalidJson,
        Err(_) => RequestStatus::OtherError,
    }
}

#[cfg(test)]
mod tests {
    use super::provider_safe_validation_error;

    #[test]
    fn provider_validation_errors_omit_protected_span_kinds() {
        let error = "error: protected span missing: https://example.com [kind=url]; \
                     warning: protected span missing: E=mc^2 [kind=math]";

        assert_eq!(
            provider_safe_validation_error(error),
            "error: protected span missing: https://example.com; \
             warning: protected span missing: E=mc^2"
        );
    }
}
