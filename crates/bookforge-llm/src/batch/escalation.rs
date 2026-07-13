use super::*;

#[derive(Default)]
pub(super) struct TruncationAlertState {
    unresolved_after_escalation: usize,
    alert_emitted: bool,
}

impl TruncationAlertState {
    pub(super) fn observe_resolved(&mut self) {
        self.unresolved_after_escalation = 0;
    }

    pub(super) fn observe_unresolved(&mut self, progress: &Arc<dyn bookforge_core::ProgressSink>) {
        self.unresolved_after_escalation = self.unresolved_after_escalation.saturating_add(1);
        if self.alert_emitted || self.unresolved_after_escalation < SYSTEMIC_TRUNCATION_ALERT_AFTER
        {
            return;
        }
        self.alert_emitted = true;
        progress.emit(bookforge_core::ProgressEvent::Warning {
            kind: "systemic_truncation".to_string(),
            message: "output budget repeatedly exhausted after escalation; raise --batch-max-output-tokens, lower --batch-max-items, or try a different model".to_string(),
            timestamp_ms: bookforge_core::progress::now_ms(),
        });
    }
}

pub(super) fn batch_max_output_tokens(
    batch: &TranslationBatch,
    profile: TranslationProfile,
    reasoning: bool,
    extended_output: bool,
) -> u32 {
    let base_multiplier = match batch.mode {
        BatchMode::Plain => 3,
        BatchMode::MarkerSafe => 4,
        BatchMode::RunPreserving => 5,
        BatchMode::TurboTextOnly => 2,
    };
    let multiplier = if reasoning {
        base_multiplier * 3
    } else {
        base_multiplier
    };
    // JSON output has a fixed envelope per item (ID, keys, quoting, commas)
    // that source-token estimates do not capture. Without this allowance,
    // batches of many short labels can receive a 512-token budget and
    // repeatedly truncate even though their prose payload is tiny.
    let envelope = 128u32.saturating_add((batch.items.len() as u32).saturating_mul(64));
    let estimate = (batch.token_estimate as u32)
        .saturating_mul(multiplier)
        .saturating_add(envelope);
    let max = if profile == TranslationProfile::FreeTier {
        if reasoning { 8_192 } else { 4_096 }
    } else if extended_output {
        32_768
    } else {
        if reasoning { 32_768 } else { 16_384 }
    };
    estimate.clamp(512, max)
}

pub(super) fn capped_batch_max_output_tokens(
    batch: &TranslationBatch,
    config: &TranslationRunConfig,
    reasoning: bool,
) -> u32 {
    let extended_output = config.provider.eq_ignore_ascii_case("deepseek");
    let computed = batch_max_output_tokens(batch, config.profile, reasoning, extended_output);
    let user_cap = config.batch_max_output_tokens.or(config.max_output_tokens);
    bookforge_core::config::cap_output_tokens(
        computed,
        batch.token_estimate,
        config.model_context_tokens,
        user_cap,
    )
}

pub(super) fn next_escalated_batch_max_output_tokens(
    current: u32,
    batch: &TranslationBatch,
    config: &TranslationRunConfig,
    reasoning: bool,
) -> Option<u32> {
    let ceiling = batch_output_token_ceiling(batch, config, reasoning);
    let bumped = current.saturating_mul(2).max(current.saturating_add(2_048));
    let next = bumped.min(ceiling);
    (next > current).then_some(next)
}

fn batch_output_token_ceiling(
    batch: &TranslationBatch,
    config: &TranslationRunConfig,
    reasoning: bool,
) -> u32 {
    let extended_output = config.provider.eq_ignore_ascii_case("deepseek");
    let ceiling = if config.profile == TranslationProfile::FreeTier {
        if reasoning { 8_192 } else { 4_096 }
    } else if extended_output || reasoning {
        32_768
    } else {
        16_384
    };
    bookforge_core::config::cap_output_tokens(
        ceiling,
        batch.token_estimate,
        config.model_context_tokens,
        None,
    )
}
