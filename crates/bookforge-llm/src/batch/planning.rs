use super::*;

impl BatchSizer {
    pub fn new(target_tokens: usize, max_items: usize) -> Self {
        Self::new_with_progress(target_tokens, max_items, None)
    }

    pub fn with_progress(
        target_tokens: usize,
        max_items: usize,
        progress: Arc<dyn bookforge_core::ProgressSink>,
    ) -> Self {
        Self::new_with_progress(target_tokens, max_items, Some(progress))
    }

    pub fn new_with_progress(
        target_tokens: usize,
        max_items: usize,
        progress: Option<Arc<dyn bookforge_core::ProgressSink>>,
    ) -> Self {
        let mut modes = HashMap::new();
        for mode in [
            BatchMode::Plain,
            BatchMode::TurboTextOnly,
            BatchMode::MarkerSafe,
            BatchMode::RunPreserving,
        ] {
            modes.insert(
                mode,
                BatchModeSizing::for_mode(mode, target_tokens, max_items),
            );
        }
        Self {
            modes,
            default_target_tokens: target_tokens,
            default_max_items: max_items,
            progress,
        }
    }

    pub fn target_tokens(&self) -> usize {
        self.target_tokens_for_mode(BatchMode::Plain)
    }

    pub fn max_items(&self) -> usize {
        self.max_items_for_mode(BatchMode::Plain)
    }

    pub fn target_tokens_for_mode(&self, mode: BatchMode) -> usize {
        self.modes
            .get(&mode)
            .map(|state| state.target_tokens)
            .unwrap_or_else(|| {
                self.default_target_tokens
                    .clamp(mode.min_tokens(), mode.max_tokens())
            })
    }

    pub fn max_items_for_mode(&self, mode: BatchMode) -> usize {
        self.modes
            .get(&mode)
            .map(|state| state.max_items)
            .unwrap_or_else(|| {
                self.default_max_items
                    .clamp(mode.min_items(), mode.max_items_cap())
            })
    }

    fn emit_change(
        &self,
        reason: &str,
        prev_target: usize,
        new_target: usize,
        prev_max: usize,
        new_max: usize,
    ) {
        if let Some(ref p) = self.progress {
            p.emit(bookforge_core::ProgressEvent::BatchSizingChanged {
                batch_id: None,
                previous_target: prev_target,
                new_target,
                previous_max_items: prev_max,
                new_max_items: new_max,
                reason: reason.to_string(),
                timestamp_ms: bookforge_core::progress::now_ms(),
            });
        }
    }

    pub fn on_truncation(&mut self) {
        self.on_truncation_for_mode(BatchMode::Plain);
    }

    pub fn on_truncation_for_mode(&mut self, mode: BatchMode) {
        if let Some((prev_target, new_target, prev_max, new_max)) =
            self.decrease_mode(mode, BatchSizingObservation::Truncation, 0.65, 0.75)
        {
            self.emit_change("truncation", prev_target, new_target, prev_max, new_max);
        }
    }

    pub fn on_invalid_json(&mut self) {
        self.on_invalid_json_for_mode(BatchMode::Plain);
    }

    pub fn on_invalid_json_for_mode(&mut self, mode: BatchMode) {
        if let Some((prev_target, new_target, prev_max, new_max)) =
            self.decrease_mode(mode, BatchSizingObservation::InvalidJson, 0.75, 0.85)
        {
            self.emit_change("invalid_json", prev_target, new_target, prev_max, new_max);
        }
    }

    pub fn on_p95_high(&mut self) {
        self.on_high_latency_for_mode(BatchMode::Plain, BATCH_SIZER_TARGET_P95_LATENCY_MS + 1);
    }

    pub fn on_high_latency_for_mode(&mut self, mode: BatchMode, latency_ms: u64) {
        if let Some((prev_target, new_target, prev_max, new_max)) = self.decrease_mode(
            mode,
            BatchSizingObservation::HighLatency { latency_ms },
            0.85,
            1.0,
        ) {
            self.emit_change("high_latency", prev_target, new_target, prev_max, new_max);
        }
    }

    pub fn on_success(&mut self) {
        self.on_success_for_mode(BatchMode::Plain, 0);
    }

    pub fn on_success_for_mode(&mut self, mode: BatchMode, latency_ms: u64) {
        let changed = {
            let state = self
                .modes
                .get_mut(&mode)
                .expect("all batch modes initialized");
            state.push_observation(BatchSizingObservation::Success { latency_ms });

            if state
                .p95_latency_ms()
                .is_some_and(|p95| p95 > BATCH_SIZER_TARGET_P95_LATENCY_MS)
            {
                let prev_target = state.target_tokens;
                let prev_max = state.max_items;
                if state.apply_decrease(0.85, 1.0) {
                    Some((
                        "high_latency",
                        prev_target,
                        state.target_tokens,
                        prev_max,
                        state.max_items,
                    ))
                } else {
                    None
                }
            } else if state.should_grow() {
                let prev_target = state.target_tokens;
                let prev_max = state.max_items;
                state.target_tokens = ((state.target_tokens as f64) * 1.10).round() as usize;
                state.max_items = state.max_items.saturating_add(mode.success_item_step());
                state.clamp();
                state.last_increase = Some(Instant::now());
                Some((
                    "stable_success",
                    prev_target,
                    state.target_tokens,
                    prev_max,
                    state.max_items,
                ))
            } else {
                None
            }
        };

        if let Some((reason, prev_target, new_target, prev_max, new_max)) = changed {
            self.emit_change(reason, prev_target, new_target, prev_max, new_max);
        }
    }

    fn decrease_mode(
        &mut self,
        mode: BatchMode,
        observation: BatchSizingObservation,
        target_factor: f64,
        item_factor: f64,
    ) -> Option<(usize, usize, usize, usize)> {
        let state = self
            .modes
            .get_mut(&mode)
            .expect("all batch modes initialized");
        state.push_observation(observation);
        let prev_target = state.target_tokens;
        let prev_max = state.max_items;
        state.apply_decrease(target_factor, item_factor).then_some((
            prev_target,
            state.target_tokens,
            prev_max,
            state.max_items,
        ))
    }
}

impl BatchModeSizing {
    fn for_mode(mode: BatchMode, initial_target_tokens: usize, initial_max_items: usize) -> Self {
        let mut state = Self {
            target_tokens: initial_target_tokens,
            max_items: initial_max_items,
            initial_target_tokens,
            initial_max_items,
            // Explicit user/runtime limits below the adaptive defaults are
            // still authoritative. They become the floor for this sizing
            // epoch; adaptation may grow from them but must not silently
            // clamp them upward on construction.
            min_tokens: initial_target_tokens.min(mode.min_tokens()).max(1),
            max_tokens: mode.max_tokens(),
            min_items: initial_max_items.min(mode.min_items()).max(1),
            max_items_cap: mode.max_items_cap(),
            recent: VecDeque::new(),
            last_increase: None,
            last_decrease: None,
        };
        state.clamp();
        state
    }

    fn clamp(&mut self) {
        self.target_tokens = self.target_tokens.clamp(self.min_tokens, self.max_tokens);
        self.max_items = self.max_items.clamp(self.min_items, self.max_items_cap);
    }

    fn push_observation(&mut self, observation: BatchSizingObservation) {
        self.recent.push_back(observation);
        while self.recent.len() > BATCH_SIZER_WINDOW {
            self.recent.pop_front();
        }
    }

    fn success_rate(&self) -> f64 {
        if self.recent.is_empty() {
            return 0.0;
        }
        let success_count = self
            .recent
            .iter()
            .filter(|obs| matches!(obs, BatchSizingObservation::Success { .. }))
            .count();
        success_count as f64 / self.recent.len() as f64
    }

    fn has_recent_truncation_or_invalid_json(&self) -> bool {
        self.recent.iter().any(|obs| {
            matches!(
                obs,
                BatchSizingObservation::Truncation | BatchSizingObservation::InvalidJson
            )
        })
    }

    fn p95_latency_ms(&self) -> Option<u64> {
        let mut latencies = self
            .recent
            .iter()
            .filter_map(|obs| match obs {
                BatchSizingObservation::Success { latency_ms }
                | BatchSizingObservation::HighLatency { latency_ms } => Some(*latency_ms),
                _ => None,
            })
            .collect::<Vec<_>>();
        if latencies.is_empty() {
            return None;
        }
        latencies.sort_unstable();
        let idx = ((latencies.len() as f64) * 0.95).ceil() as usize;
        Some(latencies[idx.saturating_sub(1).min(latencies.len() - 1)])
    }

    fn should_grow(&self) -> bool {
        if self.recent.len() < BATCH_SIZER_WINDOW {
            return false;
        }
        if self.success_rate() < BATCH_SIZER_STABLE_SUCCESS_THRESHOLD {
            return false;
        }
        if self.has_recent_truncation_or_invalid_json() {
            return false;
        }
        if self
            .p95_latency_ms()
            .is_some_and(|p95| p95 > BATCH_SIZER_TARGET_P95_LATENCY_MS)
        {
            return false;
        }
        self.last_increase
            .map(|last| last.elapsed() >= BATCH_SIZER_INCREASE_INTERVAL)
            .unwrap_or(true)
    }

    fn apply_decrease(&mut self, target_factor: f64, item_factor: f64) -> bool {
        if self
            .last_decrease
            .map(|last| last.elapsed() < BATCH_SIZER_DECREASE_INTERVAL)
            .unwrap_or(false)
        {
            return false;
        }
        let prev_target = self.target_tokens;
        let prev_items = self.max_items;
        self.target_tokens = ((self.target_tokens as f64) * target_factor).floor() as usize;
        self.max_items = ((self.max_items as f64) * item_factor).floor() as usize;
        self.clamp();
        self.last_decrease = Some(Instant::now());
        self.target_tokens != prev_target || self.max_items != prev_items
    }
}

impl BatchMode {
    fn min_tokens(self) -> usize {
        match self {
            Self::Plain | Self::TurboTextOnly => 4_000,
            Self::MarkerSafe => 2_000,
            Self::RunPreserving => 1_000,
        }
    }

    fn max_tokens(self) -> usize {
        match self {
            Self::Plain | Self::TurboTextOnly => 32_000,
            Self::MarkerSafe => 16_000,
            Self::RunPreserving => 8_000,
        }
    }

    fn min_items(self) -> usize {
        match self {
            Self::Plain | Self::TurboTextOnly => 16,
            Self::MarkerSafe => 8,
            Self::RunPreserving => 4,
        }
    }

    fn max_items_cap(self) -> usize {
        match self {
            Self::Plain | Self::TurboTextOnly => 256,
            Self::MarkerSafe => 128,
            Self::RunPreserving => 64,
        }
    }

    fn success_item_step(self) -> usize {
        match self {
            Self::Plain | Self::TurboTextOnly => 16,
            Self::MarkerSafe => 8,
            Self::RunPreserving => 4,
        }
    }
}

pub fn build_translation_batches(
    segments: &[Segment],
    config: &BatchConfig,
    profile: TranslationProfile,
) -> Vec<TranslationBatch> {
    if !config.enabled {
        return Vec::new();
    }

    let turbo = profile == TranslationProfile::TurboTextOnly;

    let mut items: Vec<TranslationBatchItem> = Vec::new();
    let mut ordinal = 0usize;

    for segment in segments {
        for block in &segment.source.blocks {
            let (source_text, required_markers, protected_spans) = if turbo {
                (
                    // Keep the marker-bearing source internally so the parser can
                    // restore valid inline templates after a text-only response.
                    // Rendering removes the markers before sending text to the LLM.
                    block.text.clone(),
                    Vec::new(),
                    block.protected_spans.clone(),
                )
            } else {
                (
                    block.text.clone(),
                    bookforge_core::marker::marker_ids_in_text(&block.text),
                    block.protected_spans.clone(),
                )
            };

            items.push(TranslationBatchItem {
                item_id: format!("{}:{}", segment.id.0, block.block_id.0),
                segment_id: segment.id.clone(),
                section_id: segment.section_id.clone(),
                block_id: block.block_id.clone(),
                ordinal,
                kind: block.kind.clone(),
                source_text,
                text_runs: block.text_runs.clone(),
                protected_spans,
                required_markers,
                checksum: segment.checksum.clone(),
            });
            ordinal += 1;
        }
    }

    group_batches(items, config, turbo.then_some(BatchMode::TurboTextOnly))
}

pub fn account_for_batch_prompt_overhead(
    batches: Vec<TranslationBatch>,
    config: &BatchConfig,
    run_config: &TranslationRunConfig,
) -> Vec<TranslationBatch> {
    let target_tokens = mode_target_tokens(config.target_tokens);
    batches
        .into_iter()
        .flat_map(|batch| {
            let token_limit = target_tokens
                .get(&batch.mode)
                .copied()
                .unwrap_or(config.target_tokens);
            repack_batch_with_config(batch, token_limit, config.max_items, Some(run_config))
        })
        .collect()
}

fn group_batches(
    items: Vec<TranslationBatchItem>,
    config: &BatchConfig,
    forced_mode: Option<BatchMode>,
) -> Vec<TranslationBatch> {
    // Partition items by (section_id, mode) before token-budget packing.
    // Section partitioning is the invariant that lets the sliding-context
    // fence work in batch mode: a batch never crosses a chapter boundary,
    // so awaiting context for the batch's earliest segment can never
    // deadlock on a sibling item in the same batch.
    let mut section_mode_groups: HashMap<
        (bookforge_core::ir::SectionId, BatchMode),
        Vec<TranslationBatchItem>,
    > = HashMap::new();
    for item in items {
        let key = (
            item.section_id.clone(),
            forced_mode.unwrap_or_else(|| item.mode()),
        );
        section_mode_groups.entry(key).or_default().push(item);
    }

    // Walk groups in (section ordinal, mode) order so the output `batches`
    // vec ends up ordered as the source document reads. The scheduler relies
    // on this to dispatch earlier sections first.
    let mut ordered_keys: Vec<(bookforge_core::ir::SectionId, BatchMode)> =
        section_mode_groups.keys().cloned().collect();
    ordered_keys.sort_by(|a, b| {
        let section_a = section_mode_groups[a]
            .iter()
            .map(|item| item.ordinal)
            .min()
            .unwrap_or(usize::MAX);
        let section_b = section_mode_groups[b]
            .iter()
            .map(|item| item.ordinal)
            .min()
            .unwrap_or(usize::MAX);
        section_a
            .cmp(&section_b)
            .then_with(|| (a.1 as u8).cmp(&(b.1 as u8)))
    });

    let target_tokens = mode_target_tokens(config.target_tokens);
    let mut batches = Vec::new();
    let mut batch_ordinal = 0usize;

    for key in ordered_keys {
        let (section_id, mode) = key.clone();
        let group_items = section_mode_groups.remove(&key).unwrap_or_default();
        let token_limit = target_tokens
            .get(&mode)
            .copied()
            .unwrap_or(config.target_tokens);
        let max_items = config.max_items;

        let mut current: Vec<TranslationBatchItem> = Vec::new();
        let mut current_tokens = 0usize;

        for item in group_items {
            let item_tokens = token_estimate(&item.source_text);
            let would_exceed_tokens =
                !current.is_empty() && current_tokens + item_tokens > token_limit;
            let would_exceed_items = max_items > 0 && current.len() >= max_items;

            if would_exceed_tokens || would_exceed_items {
                let batch = make_batch(
                    format!("batch_{:04}", batch_ordinal),
                    batch_ordinal,
                    mode,
                    std::mem::take(&mut current),
                    current_tokens,
                    section_id.clone(),
                );
                batches.push(batch);
                batch_ordinal += 1;
                current_tokens = 0;
            }

            current_tokens += item_tokens;
            current.push(item);
        }

        if !current.is_empty() {
            let batch = make_batch(
                format!("batch_{:04}", batch_ordinal),
                batch_ordinal,
                mode,
                current,
                current_tokens,
                section_id.clone(),
            );
            batches.push(batch);
            batch_ordinal += 1;
        }
    }

    batches
}

fn make_batch(
    id: String,
    ordinal: usize,
    mode: BatchMode,
    items: Vec<TranslationBatchItem>,
    token_estimate: usize,
    section_id: bookforge_core::ir::SectionId,
) -> TranslationBatch {
    TranslationBatch {
        id,
        ordinal,
        mode,
        kind: BatchKind::Translation,
        items,
        token_estimate,
        section_id,
    }
}

fn mode_target_tokens(base: usize) -> HashMap<BatchMode, usize> {
    let mut map = HashMap::new();
    map.insert(BatchMode::Plain, base);
    map.insert(BatchMode::MarkerSafe, base.min(10_000));
    map.insert(BatchMode::RunPreserving, base.min(4_000));
    map.insert(BatchMode::TurboTextOnly, base);
    map
}

pub(super) fn token_estimate(text: &str) -> usize {
    let chars = text.chars().count();
    if chars == 0 {
        return 0;
    }
    (chars / 4).max(1)
}

fn item_token_estimate(
    item: &TranslationBatchItem,
    config: Option<&TranslationRunConfig>,
) -> usize {
    let mut estimate = token_estimate(&item.source_text).max(1);
    let Some(config) = config else {
        return estimate;
    };

    if let Some(guidance) = config.glossary.guidance_by_segment.get(&item.segment_id.0) {
        estimate += token_estimate("retry_guidance") + token_estimate(guidance);
    }
    estimate
}

fn batch_fixed_token_estimate(
    items: &[TranslationBatchItem],
    config: Option<&TranslationRunConfig>,
) -> usize {
    config
        .map(|config| token_estimate(&super::rendering::render_batch_prompt_extra(items, config)))
        .unwrap_or(0)
}

fn batch_token_estimate(
    items: &[TranslationBatchItem],
    config: Option<&TranslationRunConfig>,
) -> usize {
    batch_fixed_token_estimate(items, config)
        + items
            .iter()
            .map(|item| item_token_estimate(item, config))
            .sum::<usize>()
}

/// Deterministic per-item validation for text-mode batch responses. The
/// markers that must survive are the ones present in THIS block's source;
/// protected spans are already block-scoped.
/// Failures flow into the normal repair/failure pipeline instead of the
/// translation being silently patched up.
pub fn split_batch(batch: &TranslationBatch) -> Vec<TranslationBatch> {
    split_batch_with_config(batch, None)
}

pub(super) fn split_batch_with_config(
    batch: &TranslationBatch,
    config: Option<&TranslationRunConfig>,
) -> Vec<TranslationBatch> {
    if batch.items.len() <= 1 {
        return vec![batch.clone()];
    }
    let mid = batch.items.len() / 2;
    let (left, right) = batch.items.split_at(mid);
    let mut batches = Vec::new();
    if !left.is_empty() {
        batches.push(make_batch(
            format!("{}_split_0", batch.id),
            batch.ordinal * 2,
            batch.mode,
            left.to_vec(),
            batch_token_estimate(left, config),
            batch.section_id.clone(),
        ));
    }
    if !right.is_empty() {
        batches.push(make_batch(
            format!("{}_split_1", batch.id),
            batch.ordinal * 2 + 1,
            batch.mode,
            right.to_vec(),
            batch_token_estimate(right, config),
            batch.section_id.clone(),
        ));
    }
    batches
}

pub(super) fn take_batch_output_override(
    overrides_by_item: &mut HashMap<String, u32>,
    batch: &TranslationBatch,
) -> Option<u32> {
    batch
        .items
        .iter()
        .filter_map(|item| overrides_by_item.remove(&item.item_id))
        .max()
}

pub(super) fn set_batch_output_override(
    overrides_by_item: &mut HashMap<String, u32>,
    batch: &TranslationBatch,
    max_output_tokens: u32,
) {
    for item in &batch.items {
        overrides_by_item.insert(item.item_id.clone(), max_output_tokens);
    }
}

pub(super) fn increment_batch_item_attempts(
    attempts_by_item: &mut HashMap<String, usize>,
    batch: &TranslationBatch,
) -> usize {
    let next = batch
        .items
        .iter()
        .filter_map(|item| attempts_by_item.get(&item.item_id).copied())
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    for item in &batch.items {
        attempts_by_item.insert(item.item_id.clone(), next);
    }
    next
}

pub(super) fn adaptive_sizer_mut<'a>(
    runtime_sizer: &'a mut Option<(u64, bool, BatchSizer)>,
    fallback: Option<&'a mut BatchSizer>,
) -> Option<&'a mut BatchSizer> {
    match runtime_sizer {
        Some((_, true, sizer)) => Some(sizer),
        Some((_, false, _)) => None,
        None => fallback,
    }
}

pub(super) fn normalize_batch_for_current_sizer(
    batch: TranslationBatch,
    sizer: Option<&BatchSizer>,
    config: Option<&TranslationRunConfig>,
) -> Vec<TranslationBatch> {
    let Some(sizer) = sizer else {
        return vec![with_configured_token_estimate(batch, config)];
    };
    let target_tokens = sizer.target_tokens_for_mode(batch.mode);
    let max_items = sizer.max_items_for_mode(batch.mode);
    let batch = with_configured_token_estimate(batch, config);
    if batch.token_estimate <= target_tokens && batch.items.len() <= max_items {
        return vec![batch];
    }
    repack_batch_with_config(batch, target_tokens, max_items, config)
}

pub(super) fn repartition_pending_batches(
    pending: &mut VecDeque<TranslationBatch>,
    sizer: &BatchSizer,
    config: Option<&TranslationRunConfig>,
    revision: u64,
) {
    if pending.is_empty() {
        return;
    }

    let mut groups = Vec::<TranslationBatch>::new();
    for batch in pending.drain(..) {
        if let Some(group) = groups.last_mut()
            && group.mode == batch.mode
            && group.kind == batch.kind
            && group.section_id == batch.section_id
        {
            group.items.extend(batch.items);
            group.token_estimate = batch_token_estimate(&group.items, config);
            continue;
        }
        groups.push(batch);
    }

    let mut rebuilt = VecDeque::new();
    for (group_index, mut group) in groups.into_iter().enumerate() {
        group.id = format!("runtime_r{revision}_{group_index}");
        group.token_estimate = batch_token_estimate(&group.items, config);
        let target_tokens = sizer.target_tokens_for_mode(group.mode);
        let max_items = sizer.max_items_for_mode(group.mode);
        rebuilt.extend(repack_batch_with_config(
            group,
            target_tokens,
            max_items,
            config,
        ));
    }
    *pending = rebuilt;
}

#[cfg(test)]
pub(super) fn repack_batch(
    batch: TranslationBatch,
    target_tokens: usize,
    max_items: usize,
) -> Vec<TranslationBatch> {
    repack_batch_with_config(batch, target_tokens, max_items, None)
}

fn repack_batch_with_config(
    batch: TranslationBatch,
    target_tokens: usize,
    max_items: usize,
    config: Option<&TranslationRunConfig>,
) -> Vec<TranslationBatch> {
    let target_tokens = target_tokens.max(1);
    let max_items = max_items.max(1);
    let mut out = Vec::new();
    let mut current_items = Vec::new();
    let mut current_tokens = 0usize;
    let mut part = 0usize;
    let base_id = batch.id;
    let base_ordinal = batch.ordinal;
    let mode = batch.mode;
    let kind = batch.kind;
    let section_id = batch.section_id;

    for item in batch.items {
        let would_exceed_items = current_items.len() >= max_items;
        current_items.push(item);
        let candidate_tokens = batch_token_estimate(&current_items, config);
        let would_exceed_tokens = current_items.len() > 1 && candidate_tokens > target_tokens;
        if would_exceed_items || would_exceed_tokens {
            let item = current_items
                .pop()
                .expect("candidate batch contains the item just added");
            out.push(TranslationBatch {
                id: format!("{base_id}_adaptive_{part}"),
                ordinal: base_ordinal * 1000 + part,
                mode,
                kind,
                items: std::mem::take(&mut current_items),
                token_estimate: current_tokens,
                section_id: section_id.clone(),
            });
            current_items.push(item);
            current_tokens = batch_token_estimate(&current_items, config);
            part += 1;
        } else {
            current_tokens = candidate_tokens;
        }
    }

    if !current_items.is_empty() {
        out.push(TranslationBatch {
            id: format!("{base_id}_adaptive_{part}"),
            ordinal: base_ordinal * 1000 + part,
            mode,
            kind,
            items: current_items,
            token_estimate: current_tokens,
            section_id,
        });
    }
    out
}

fn with_configured_token_estimate(
    mut batch: TranslationBatch,
    config: Option<&TranslationRunConfig>,
) -> TranslationBatch {
    batch.token_estimate = batch_token_estimate(&batch.items, config);
    batch
}
