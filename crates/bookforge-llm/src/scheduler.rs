use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bookforge_core::{
    config::{BatchConfig, ContextScope, ResolvedRunSettings, TranslationProfile},
    glossary::{GlossaryFormat, GlossaryPromptTerm},
    ir::{BlockId, ProtectedSpan},
    scheduler::SchedulerConfig,
    segment::{BlockTranslation, Segment, SegmentBlock, SegmentId, SegmentStatus},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    sync::{mpsc, watch},
    task::JoinSet,
};

use crate::{
    concurrency::{PauseSignal, PauseState},
    prompt::{PromptLibrary, PromptTemplate, Substitutions},
    provider::{
        CompletionRequest, FinishReason, LlmError, LlmProvider, RequestMetadata, ResponseFormat,
        Result,
    },
};

#[derive(Debug, Clone)]
pub struct TranslationRunConfig {
    pub source_language: Option<String>,
    pub target_language: String,
    pub provider: String,
    pub model: String,
    pub prompt_version: String,
    pub temperature: f32,
    pub scheduler: SchedulerConfig,
    pub profile: TranslationProfile,
    pub model_context_tokens: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub batch_max_output_tokens: Option<u32>,
    pub compact_prompts: bool,
    pub glossary: GlossaryRunConfig,
    pub context: ContextRunConfig,
    /// In-memory completion fence for sliding-context injection. Built by
    /// the caller from the full segments list and pre-populated with any
    /// cache hits before translation starts. Optional; when `None` the
    /// scheduler runs without context injection even if `context.window`
    /// is non-zero (degrades silently with a `tracing::warn!`).
    pub context_registry: Option<Arc<ContextRegistry>>,
    /// Merged style sheet pre-rendered as a prompt block. The scheduler
    /// substitutes `rendered_block` into the `{{style_guide_block}}`
    /// placeholder in per-segment and batch prompts. `None` = no style
    /// sheet active; renders to empty string.
    pub style: Option<StyleRunConfig>,
    /// Merged entity table pre-rendered as a prompt block. The scheduler
    /// substitutes `rendered_block` into the `{{entity_agreement_block}}`
    /// placeholder. `None` = no entities active; renders to empty string.
    pub entities: Option<EntityRunConfig>,
    /// Cooperative control signal checked before dispatching each new segment.
    pub pause_signal: Option<PauseSignal>,
    /// Optional live, cache-safe runtime settings. Each dispatcher snapshots
    /// this receiver only at a documented request/batch boundary.
    pub runtime_settings: Option<watch::Receiver<EngineRuntimeSettings>>,
}

impl TranslationRunConfig {
    pub(crate) fn request_runtime_metadata(&self) -> (Option<u64>, Option<usize>) {
        self.runtime_settings
            .as_ref()
            .map(|receiver| {
                let runtime = receiver.borrow();
                (
                    Some(runtime.revision),
                    Some(runtime.provider_max_attempts.max(1)),
                )
            })
            .unwrap_or((None, None))
    }
}

#[derive(Debug, Clone)]
pub struct EngineRuntimeSettings {
    pub revision: u64,
    pub batch: BatchConfig,
    pub batch_max_output_tokens: Option<u32>,
    pub concurrency: usize,
    pub provider_max_attempts: usize,
    pub adaptive_concurrency: bool,
}

impl EngineRuntimeSettings {
    pub fn from_resolved(revision: u64, settings: &ResolvedRunSettings) -> Self {
        Self {
            revision,
            batch: settings.batch.clone(),
            batch_max_output_tokens: settings.provider.batch_max_output_tokens,
            concurrency: settings.scheduler.concurrency.max(1),
            provider_max_attempts: settings.provider.provider_max_attempts.max(1),
            adaptive_concurrency: settings.adaptive_concurrency,
        }
    }

    pub(crate) fn frozen_receiver(self) -> watch::Receiver<Self> {
        let (_sender, receiver) = watch::channel(self);
        receiver
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyleRunConfig {
    pub rendered_block: String,
    pub fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityRunConfig {
    pub rendered_block: String,
    pub fingerprint: String,
}

/// Sliding-context injection settings. `window == 0` disables the feature
/// entirely; existing cache entries stay valid because the context block
/// is rendered as an empty string and the rendered prompt itself is not
/// part of the cache key.
///
/// `strict` controls the completion fence. When `true`, a segment waits
/// until all `window` predecessors have finished, which guarantees a full
/// context block but serializes segments within the scope (a one-file
/// EPUB translates strictly one segment at a time). When `false` (the
/// default), the segment takes whatever predecessors have already
/// completed at the moment it starts and never waits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextRunConfig {
    pub window: usize,
    pub budget_tokens: usize,
    pub scope: ContextScope,
    pub strict: bool,
}

impl Default for ContextRunConfig {
    fn default() -> Self {
        Self {
            window: 0,
            budget_tokens: 1200,
            scope: ContextScope::Chapter,
            strict: false,
        }
    }
}

impl ContextRunConfig {
    pub fn enabled(&self) -> bool {
        self.window > 0
    }
}

/// A completed prior segment, exposed to the prompt renderer as one side
/// of a (source, target) pair in the sliding-context block.
#[derive(Debug, Clone)]
pub struct CompletedContext {
    pub segment_id: SegmentId,
    pub section_id: bookforge_core::ir::SectionId,
    pub ordinal: usize,
    pub source_text: String,
    pub translated_text: String,
    pub status: SegmentStatus,
    pub source_token_estimate: usize,
}

/// In-memory completion fence for sliding-context injection.
///
/// Each segment is pre-registered before translation starts. As segments
/// complete (whether from cache, prior run, or fresh translation), they
/// publish a `CompletedContext`; waiters subscribe on the segments they
/// need and unblock once those entries land. Failed and needs-review
/// statuses are recorded so they unblock waiters but are filtered out of
/// the rendered context, per ROADMAP §6.4.
#[derive(Clone)]
pub struct ContextRegistry {
    inner: Arc<ContextRegistryInner>,
}

impl std::fmt::Debug for ContextRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContextRegistry")
            .field("segments", &self.inner.segment_index.len())
            .field("sections", &self.inner.section_order.len())
            .finish()
    }
}

struct ContextRegistryInner {
    completed: std::sync::Mutex<HashMap<SegmentId, CompletedContext>>,
    notify: tokio::sync::Notify,
    section_order: HashMap<bookforge_core::ir::SectionId, Vec<SegmentId>>,
    book_order: Vec<SegmentId>,
    segment_index: HashMap<SegmentId, SegmentLocation>,
}

#[derive(Clone)]
struct SegmentLocation {
    section_id: bookforge_core::ir::SectionId,
}

impl ContextRegistry {
    pub fn new(segments: &[Segment]) -> Self {
        let mut section_order: HashMap<bookforge_core::ir::SectionId, Vec<(usize, SegmentId)>> =
            HashMap::new();
        let mut book_order: Vec<(usize, SegmentId)> = Vec::with_capacity(segments.len());
        let mut segment_index: HashMap<SegmentId, SegmentLocation> = HashMap::new();
        for segment in segments {
            section_order
                .entry(segment.section_id.clone())
                .or_default()
                .push((segment.ordinal, segment.id.clone()));
            book_order.push((segment.ordinal, segment.id.clone()));
            segment_index.insert(
                segment.id.clone(),
                SegmentLocation {
                    section_id: segment.section_id.clone(),
                },
            );
        }
        for list in section_order.values_mut() {
            list.sort_by_key(|(ord, _)| *ord);
        }
        book_order.sort_by_key(|(ord, _)| *ord);
        let section_order: HashMap<_, Vec<SegmentId>> = section_order
            .into_iter()
            .map(|(k, v)| (k, v.into_iter().map(|(_, id)| id).collect()))
            .collect();
        let book_order: Vec<SegmentId> = book_order.into_iter().map(|(_, id)| id).collect();
        Self {
            inner: Arc::new(ContextRegistryInner {
                completed: std::sync::Mutex::new(HashMap::new()),
                notify: tokio::sync::Notify::new(),
                section_order,
                book_order,
                segment_index,
            }),
        }
    }

    /// Publish a completed context. Idempotent: late re-publishes simply
    /// overwrite the prior entry (in practice this only happens during
    /// retries, where the freshest result is preferred).
    pub fn publish(&self, ctx: CompletedContext) {
        let mut map = self
            .inner
            .completed
            .lock()
            .expect("context registry mutex poisoned");
        map.insert(ctx.segment_id.clone(), ctx);
        drop(map);
        self.inner.notify.notify_waiters();
    }

    /// Convenience: seed the registry from a `(segment, translation)` pair
    /// known to be complete before translation starts (cache hit, prior
    /// run, etc.).
    pub fn pre_populate(&self, segment: &Segment, translation: &SegmentTranslation) {
        self.publish(CompletedContext {
            segment_id: segment.id.clone(),
            section_id: segment.section_id.clone(),
            ordinal: segment.ordinal,
            source_text: segment.source.text.clone(),
            translated_text: translation.joined_text(),
            status: translation.status,
            source_token_estimate: segment.source.token_estimate,
        });
    }

    /// Convenience: seed the registry from raw (segment, translated_text, status).
    /// Used on resume to rehydrate the fence from persisted translations.
    pub fn pre_populate_text(
        &self,
        segment: &Segment,
        translated_text: impl Into<String>,
        status: SegmentStatus,
    ) {
        self.publish(CompletedContext {
            segment_id: segment.id.clone(),
            section_id: segment.section_id.clone(),
            ordinal: segment.ordinal,
            source_text: segment.source.text.clone(),
            translated_text: translated_text.into(),
            status,
            source_token_estimate: segment.source.token_estimate,
        });
    }

    /// Return the prior `config.window` segments' `CompletedContext`
    /// entries (scoped per `config.scope`) in closest-first order, filtered
    /// to successful translations and truncated to fit
    /// `config.budget_tokens`.
    ///
    /// In strict mode this waits until every prior segment has completed.
    /// In best-effort mode (the default) it returns immediately with
    /// whatever predecessors happen to be done.
    pub async fn await_context_for(
        &self,
        segment_id: &SegmentId,
        config: ContextRunConfig,
    ) -> Vec<CompletedContext> {
        if !config.enabled() {
            return Vec::new();
        }
        let Some(loc) = self.inner.segment_index.get(segment_id) else {
            return Vec::new();
        };
        let prior: Vec<SegmentId> = match config.scope {
            ContextScope::Chapter => {
                let Some(section) = self.inner.section_order.get(&loc.section_id) else {
                    return Vec::new();
                };
                prior_segments_before(section, segment_id, config.window)
            }
            ContextScope::Book => {
                prior_segments_before(&self.inner.book_order, segment_id, config.window)
            }
        };
        if prior.is_empty() {
            return Vec::new();
        }
        if !config.strict {
            let map = self
                .inner
                .completed
                .lock()
                .expect("context registry mutex poisoned");
            let succeeded: Vec<CompletedContext> = prior
                .iter()
                .filter_map(|id| map.get(id).cloned())
                .filter(|ctx| {
                    matches!(
                        ctx.status,
                        SegmentStatus::Succeeded | SegmentStatus::SkippedCached
                    )
                })
                .collect();
            return apply_context_budget(succeeded, config.budget_tokens);
        }
        loop {
            let notified = self.inner.notify.notified();
            tokio::pin!(notified);
            {
                let map = self
                    .inner
                    .completed
                    .lock()
                    .expect("context registry mutex poisoned");
                if prior.iter().all(|id| map.contains_key(id)) {
                    let succeeded: Vec<CompletedContext> = prior
                        .iter()
                        .filter_map(|id| map.get(id).cloned())
                        .filter(|ctx| {
                            matches!(
                                ctx.status,
                                SegmentStatus::Succeeded | SegmentStatus::SkippedCached
                            )
                        })
                        .collect();
                    return apply_context_budget(succeeded, config.budget_tokens);
                }
            }
            notified.await;
        }
    }
}

fn prior_segments_before(
    ordered: &[SegmentId],
    needle: &SegmentId,
    window: usize,
) -> Vec<SegmentId> {
    let Some(idx) = ordered.iter().position(|id| id == needle) else {
        return Vec::new();
    };
    if idx == 0 || window == 0 {
        return Vec::new();
    }
    let start = idx.saturating_sub(window);
    // Closest-first: walk from idx-1 down to start.
    (start..idx).rev().map(|i| ordered[i].clone()).collect()
}

fn apply_context_budget(
    closest_first: Vec<CompletedContext>,
    budget_tokens: usize,
) -> Vec<CompletedContext> {
    if budget_tokens == 0 {
        return Vec::new();
    }
    let mut total = 0usize;
    let mut kept: Vec<CompletedContext> = Vec::with_capacity(closest_first.len());
    for ctx in closest_first {
        let tokens = estimate_context_tokens(&ctx);
        if total.saturating_add(tokens) > budget_tokens {
            break;
        }
        total += tokens;
        kept.push(ctx);
    }
    kept
}

fn estimate_context_tokens(ctx: &CompletedContext) -> usize {
    let translation_tokens = ctx.translated_text.len() / 4;
    ctx.source_token_estimate.saturating_add(translation_tokens)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlossaryRunConfig {
    pub format: GlossaryFormat,
    pub entries_by_segment: HashMap<String, Vec<GlossaryPromptTerm>>,
    pub prompt_extra: Option<String>,
    /// Operator guidance for an explicit single-segment retry.
    pub guidance_by_segment: HashMap<String, String>,
}

impl Default for GlossaryRunConfig {
    fn default() -> Self {
        Self {
            format: GlossaryFormat::Json,
            entries_by_segment: HashMap::new(),
            prompt_extra: None,
            guidance_by_segment: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentTranslation {
    pub segment_id: SegmentId,
    pub ordinal: usize,
    pub block_ids: Vec<BlockId>,
    pub blocks: Vec<BlockTranslation>,
    pub checksum: String,
    pub status: SegmentStatus,
    pub template: String,
    pub error: Option<String>,
    pub input_tokens: Option<u64>,
    pub input_cached_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub tokens_estimated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QaSegmentReview {
    pub segment_id: SegmentId,
    pub verdict: String,
    pub issues: Vec<QaIssue>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QaIssue {
    pub severity: String,
    pub kind: String,
    pub message: String,
    pub source_excerpt: Option<String>,
    pub translation_excerpt: Option<String>,
}

impl SegmentTranslation {
    /// Joined translation text, derived by concatenating per-block translations
    /// with a blank line between blocks. Convenience for callers that need a
    /// single string (CLI summary, DB column).
    pub fn joined_text(&self) -> String {
        self.blocks
            .iter()
            .map(|block| block.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TranslationMode {
    Plain,
    MarkerSafe,
    RunPreserving,
}

impl TranslationMode {
    fn template_name(self) -> &'static str {
        match self {
            Self::Plain => "translate_segment",
            Self::MarkerSafe => "translate_marker_safe",
            Self::RunPreserving => "translate_run_preserving",
        }
    }

    fn temperature_default(self) -> f32 {
        match self {
            Self::Plain => 0.2,
            Self::MarkerSafe => 0.1,
            Self::RunPreserving => 0.0,
        }
    }
}

#[derive(Debug, Deserialize)]
struct PlainTranslationResponse {
    segment_id: String,
    translation: String,
}

#[derive(Debug, Deserialize)]
struct MarkerSafeTranslationResponse {
    segment_id: String,
    blocks: Vec<MarkerSafeBlock>,
}

#[derive(Debug, Deserialize)]
struct MarkerSafeBlock {
    block_id: String,
    translation: String,
}

#[derive(Debug, Deserialize)]
struct RunPreservingTranslationResponse {
    segment_id: String,
    blocks: Vec<RunPreservingBlock>,
}

#[derive(Debug, Deserialize)]
struct RunPreservingBlock {
    block_id: String,
    translated_runs: Vec<TranslatedRun>,
}

#[derive(Debug, Deserialize)]
struct TranslatedRun {
    id: String,
    text: String,
}

pub async fn translate_segments<P>(
    provider: P,
    segments: &[Segment],
    config: &TranslationRunConfig,
) -> Result<Vec<SegmentTranslation>>
where
    P: LlmProvider,
{
    translate_segments_with_callback(provider, segments, config, |_| Ok(()), None).await
}

pub async fn translate_segments_with_callback<P, F>(
    provider: P,
    segments: &[Segment],
    config: &TranslationRunConfig,
    mut on_translation: F,
    finalized_tx: Option<mpsc::Sender<SegmentTranslation>>,
) -> Result<Vec<SegmentTranslation>>
where
    P: LlmProvider,
    F: FnMut(&SegmentTranslation) -> Result<()>,
{
    translate_segments_with_control(
        provider,
        segments,
        config,
        &mut on_translation,
        finalized_tx,
        |_| Ok(()),
    )
    .await
}

pub async fn translate_segments_with_control<P, F, B>(
    provider: P,
    segments: &[Segment],
    config: &TranslationRunConfig,
    mut on_translation: F,
    finalized_tx: Option<mpsc::Sender<SegmentTranslation>>,
    mut on_control_boundary: B,
) -> Result<Vec<SegmentTranslation>>
where
    P: LlmProvider,
    F: FnMut(&SegmentTranslation) -> Result<()>,
    B: FnMut(&PauseSignal) -> Result<()>,
{
    if config.scheduler.concurrency == 0 {
        return Err(LlmError::Provider(
            "scheduler concurrency must be greater than zero".to_string(),
        ));
    }

    let library = Arc::new(PromptLibrary::global().clone());
    let provider = Arc::new(provider);
    let config = Arc::new(config.clone());
    let pause_signal = config.pause_signal.clone();
    let mut dispatch_segments = segments.to_vec();
    dispatch_segments.sort_by_key(|segment| segment.ordinal);
    let mut tasks = JoinSet::new();
    let mut next_index = 0usize;
    let mut stop_dispatch = false;

    let mut translations = Vec::with_capacity(segments.len());
    loop {
        while !stop_dispatch && next_index < dispatch_segments.len() {
            if let Some(signal) = pause_signal.as_ref() {
                on_control_boundary(signal)?;
                match signal.state() {
                    PauseState::Running => {}
                    PauseState::Stopped => {
                        stop_dispatch = true;
                        break;
                    }
                    PauseState::Paused if tasks.is_empty() => {
                        if wait_for_resume_or_stop(signal, &mut on_control_boundary).await?
                            == PauseState::Stopped
                        {
                            stop_dispatch = true;
                            break;
                        }
                    }
                    PauseState::Paused => break,
                }
            }
            // A paused dispatcher may have waited while the watcher published
            // a newer revision. Snapshot only after the control boundary so a
            // Resume can never release work under the pre-pause settings.
            let runtime_snapshot = config
                .runtime_settings
                .as_ref()
                .map(|receiver| receiver.borrow().clone());
            let concurrency = runtime_snapshot
                .as_ref()
                .map(|runtime| runtime.concurrency)
                .unwrap_or(config.scheduler.concurrency)
                .max(1);
            if tasks.len() >= concurrency {
                break;
            }

            let segment = dispatch_segments[next_index].clone();
            next_index += 1;
            let provider = provider.clone();
            let mut task_config = config.as_ref().clone();
            if let Some(runtime) = runtime_snapshot {
                task_config.scheduler.concurrency = runtime.concurrency.max(1);
                task_config.batch_max_output_tokens = runtime.batch_max_output_tokens;
                task_config.runtime_settings = Some(runtime.frozen_receiver());
            }
            let config = Arc::new(task_config);
            let library = library.clone();

            tasks.spawn(async move {
                // Strict mode waits for predecessors before the provider call.
                // Bounded dispatch keeps waiters from flooding the executor.
                let strict_context_pairs = if config.context.strict {
                    Some(context_pairs_for_segment(&segment, &config).await)
                } else {
                    None
                };
                let context_pairs = match strict_context_pairs {
                    Some(pairs) => pairs,
                    None => context_pairs_for_segment(&segment, &config).await,
                };
                let translation =
                    translate_one(provider, library, segment.clone(), &config, context_pairs).await;
                if let Some(registry) = config.context_registry.as_deref() {
                    registry.pre_populate(&segment, &translation);
                }
                translation
            });
        }

        if tasks.is_empty() {
            if stop_dispatch || next_index >= dispatch_segments.len() {
                break;
            }
            continue;
        }

        let Some(result) = tasks.join_next().await else {
            continue;
        };
        let translation = result.map_err(|err| LlmError::Provider(err.to_string()))?;
        if let Some(ref tx) = finalized_tx {
            tx.send(translation.clone())
                .await
                .map_err(|_| LlmError::Provider("finalized segment channel closed".to_string()))?;
        }
        on_translation(&translation)?;
        translations.push(translation);
    }

    translations.sort_by_key(|translation| translation.ordinal);
    Ok(translations)
}

async fn wait_for_resume_or_stop<B>(
    signal: &PauseSignal,
    on_control_boundary: &mut B,
) -> Result<PauseState>
where
    B: FnMut(&PauseSignal) -> Result<()>,
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

pub async fn qa_segments<P>(
    provider: P,
    segments: &[Segment],
    translations: &[SegmentTranslation],
    config: &TranslationRunConfig,
) -> Vec<QaSegmentReview>
where
    P: LlmProvider,
{
    let library = PromptLibrary::global();
    let by_segment = segments
        .iter()
        .map(|segment| (segment.id.0.as_str(), segment))
        .collect::<std::collections::HashMap<_, _>>();
    let mut reviews = Vec::new();

    for translation in translations {
        if translation.status != SegmentStatus::Succeeded {
            continue;
        }
        let Some(segment) = by_segment.get(translation.segment_id.0.as_str()) else {
            continue;
        };
        match request_qa(&provider, library, segment, translation, config).await {
            Ok(review) => reviews.push(review),
            Err(error) => reviews.push(QaSegmentReview {
                segment_id: translation.segment_id.clone(),
                verdict: "warn".to_string(),
                issues: vec![QaIssue {
                    severity: "medium".to_string(),
                    kind: "qa_request_failed".to_string(),
                    message: format!("QA pass failed: {error}"),
                    source_excerpt: None,
                    translation_excerpt: None,
                }],
            }),
        }
    }

    reviews
}

#[derive(Debug, Deserialize)]
struct QaResponse {
    segment_id: String,
    verdict: String,
    issues: Vec<QaIssue>,
}

async fn request_qa<P>(
    provider: &P,
    library: &PromptLibrary,
    segment: &Segment,
    translation: &SegmentTranslation,
    config: &TranslationRunConfig,
) -> Result<QaSegmentReview>
where
    P: LlmProvider,
{
    let rendered = render_qa_prompt(&library.qa, segment, translation, config)?;
    let (runtime_config_revision, provider_max_attempts) = config.request_runtime_metadata();
    let response = provider
        .complete(CompletionRequest {
            system: rendered.system,
            user: rendered.user,
            response_format: ResponseFormat::Json,
            temperature: 0.0,
            max_output_tokens: None,
            metadata: RequestMetadata {
                segment_id: Some(segment.id.0.clone()),
                block_ids: segment.block_ids.iter().map(|id| id.0.clone()).collect(),
                prompt_template: Some(library.qa.name.clone()),
                prompt_version: Some(library.qa.version.clone()),
                provider: Some(config.provider.clone()),
                model: Some(config.model.clone()),
                source_checksum: Some(segment.checksum.clone()),
                runtime_config_revision,
                provider_max_attempts,
            },
        })
        .await?;
    let parsed: QaResponse = serde_json::from_str(&response.content)?;
    if parsed.segment_id != segment.id.0 {
        return Err(LlmError::InvalidResponse(format!(
            "QA segment id mismatch: expected '{}', got '{}'",
            segment.id.0, parsed.segment_id
        )));
    }

    Ok(QaSegmentReview {
        segment_id: segment.id.clone(),
        verdict: parsed.verdict,
        issues: parsed.issues,
    })
}

fn render_qa_prompt(
    template: &PromptTemplate,
    segment: &Segment,
    translation: &SegmentTranslation,
    config: &TranslationRunConfig,
) -> Result<crate::prompt::Rendered> {
    let mut vars = Substitutions::new();
    vars.string(
        "source_language",
        config.source_language.as_deref().unwrap_or("auto"),
    )
    .string("target_language", &config.target_language)
    .string("segment_id", &segment.id.0)
    .string(
        "book_title",
        segment.metadata.book_title.as_deref().unwrap_or(""),
    )
    .string(
        "section_title",
        segment.metadata.section_title.as_deref().unwrap_or(""),
    )
    .json("glossary_json", &Value::Array(Vec::new()))
    .raw("glossary_block_prose", "")
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
    .raw("prompt_extra", "")
    .raw("source_text", &segment.source.text)
    .raw("translation_text", translation.joined_text());

    template
        .render(&vars)
        .map_err(|err| LlmError::Provider(format!("QA prompt render failed: {err}")))
}

async fn translate_one<P>(
    provider: Arc<P>,
    library: Arc<PromptLibrary>,
    segment: Segment,
    config: &TranslationRunConfig,
    context_pairs: Vec<CompletedContext>,
) -> SegmentTranslation
where
    P: LlmProvider,
{
    let mode = select_mode(&segment);
    let attempts = config.scheduler.max_attempts.max(1);
    let mut last_error: Option<LlmError> = None;
    let mut accum_in: u64 = 0;
    let mut accum_cached_in: u64 = 0;
    let mut accum_out: u64 = 0;

    for _ in 0..attempts {
        let retry_context = last_error.as_ref().map(ToString::to_string);
        let result = request_translation(
            provider.as_ref(),
            library.as_ref(),
            &segment,
            config,
            mode,
            retry_context.as_deref(),
            &context_pairs,
        )
        .await;
        match result {
            Ok(translation) => {
                let mut translation = translation;
                accum_in += translation.input_tokens.unwrap_or(0);
                accum_cached_in += translation.input_cached_tokens.unwrap_or(0);
                accum_out += translation.output_tokens.unwrap_or(0);
                translation.input_tokens = if accum_in > 0 { Some(accum_in) } else { None };
                translation.input_cached_tokens = if accum_cached_in > 0 {
                    Some(accum_cached_in)
                } else {
                    None
                };
                translation.output_tokens = if accum_out > 0 { Some(accum_out) } else { None };
                return translation;
            }
            Err(error) => {
                if is_validator_error(&error) {
                    last_error = Some(error);
                } else {
                    let tokens_in = if accum_in > 0 { Some(accum_in) } else { None };
                    let cached_in = if accum_cached_in > 0 {
                        Some(accum_cached_in)
                    } else {
                        None
                    };
                    let tokens_out = if accum_out > 0 { Some(accum_out) } else { None };
                    return failed_translation_with_tokens(
                        &segment,
                        mode,
                        error.to_string(),
                        tokens_in,
                        cached_in,
                        tokens_out,
                    );
                }
            }
        }
    }

    let mut final_mode = mode;
    if mode == TranslationMode::MarkerSafe && has_structured_runs(&segment) {
        let retry_context = last_error.as_ref().map(ToString::to_string);
        let result = request_translation(
            provider.as_ref(),
            library.as_ref(),
            &segment,
            config,
            TranslationMode::RunPreserving,
            retry_context.as_deref(),
            &context_pairs,
        )
        .await;
        match result {
            Ok(translation) => {
                let mut translation = translation;
                accum_in += translation.input_tokens.unwrap_or(0);
                accum_cached_in += translation.input_cached_tokens.unwrap_or(0);
                accum_out += translation.output_tokens.unwrap_or(0);
                translation.input_tokens = if accum_in > 0 { Some(accum_in) } else { None };
                translation.input_cached_tokens = if accum_cached_in > 0 {
                    Some(accum_cached_in)
                } else {
                    None
                };
                translation.output_tokens = if accum_out > 0 { Some(accum_out) } else { None };
                return translation;
            }
            Err(error) => {
                if is_validator_error(&error) {
                    final_mode = TranslationMode::RunPreserving;
                    last_error = Some(error);
                } else {
                    let tokens_in = if accum_in > 0 { Some(accum_in) } else { None };
                    let cached_in = if accum_cached_in > 0 {
                        Some(accum_cached_in)
                    } else {
                        None
                    };
                    let tokens_out = if accum_out > 0 { Some(accum_out) } else { None };
                    return failed_translation_with_tokens(
                        &segment,
                        TranslationMode::RunPreserving,
                        error.to_string(),
                        tokens_in,
                        cached_in,
                        tokens_out,
                    );
                }
            }
        }
    }

    let tokens_in = if accum_in > 0 { Some(accum_in) } else { None };
    let cached_in = if accum_cached_in > 0 {
        Some(accum_cached_in)
    } else {
        None
    };
    let tokens_out = if accum_out > 0 { Some(accum_out) } else { None };
    let error_message = last_error
        .map(|err| err.to_string())
        .unwrap_or_else(|| "exhausted validation retries".to_string());
    needs_review_translation_with_tokens(
        &segment,
        final_mode,
        error_message,
        tokens_in,
        cached_in,
        tokens_out,
    )
}

async fn context_pairs_for_segment(
    segment: &Segment,
    config: &TranslationRunConfig,
) -> Vec<CompletedContext> {
    match config.context_registry.as_deref() {
        Some(registry) if config.context.enabled() => {
            registry
                .await_context_for(&segment.id, config.context)
                .await
        }
        _ => Vec::new(),
    }
}

fn select_mode(segment: &Segment) -> TranslationMode {
    if segment.source.blocks.len() <= 1 && segment.constraints.preserve_markers.is_empty() {
        TranslationMode::Plain
    } else {
        TranslationMode::MarkerSafe
    }
}

async fn request_translation<P>(
    provider: &P,
    library: &PromptLibrary,
    segment: &Segment,
    config: &TranslationRunConfig,
    mode: TranslationMode,
    retry_context: Option<&str>,
    context_pairs: &[CompletedContext],
) -> Result<SegmentTranslation>
where
    P: LlmProvider,
{
    let template = match mode {
        TranslationMode::Plain => &library.plain,
        TranslationMode::MarkerSafe => &library.marker_safe,
        TranslationMode::RunPreserving => &library.run_preserving,
    };
    let mut rendered = render_prompt(template, segment, config, mode, context_pairs)?;
    if let Some(retry_context) = retry_context {
        rendered
            .user
            .push_str(&validation_retry_appendix(segment, mode, retry_context));
    }
    let temperature = if config.temperature > 0.0 {
        config.temperature
    } else {
        mode.temperature_default()
    };

    let max_output_tokens = config.max_output_tokens.unwrap_or_else(|| {
        max_output_tokens(
            segment,
            mode,
            provider.is_reasoning(),
            &config.target_language,
            config.provider.eq_ignore_ascii_case("deepseek"),
        )
    });
    let (runtime_config_revision, provider_max_attempts) = config.request_runtime_metadata();
    let request = CompletionRequest {
        system: rendered.system,
        user: rendered.user,
        response_format: ResponseFormat::Json,
        temperature,
        max_output_tokens: Some(max_output_tokens),
        metadata: RequestMetadata {
            segment_id: Some(segment.id.0.clone()),
            block_ids: segment.block_ids.iter().map(|id| id.0.clone()).collect(),
            prompt_template: Some(template.name.clone()),
            prompt_version: Some(template.version.clone()),
            provider: Some(config.provider.clone()),
            model: Some(config.model.clone()),
            source_checksum: Some(segment.checksum.clone()),
            runtime_config_revision,
            provider_max_attempts,
        },
    };
    let response = provider.complete(request).await?;
    if response.finish_reason == FinishReason::Length {
        return Err(LlmError::InvalidResponse(
            "output was truncated: max_output_tokens limit reached".to_string(),
        ));
    }
    let validate_source_copy = crate::validation::should_validate_source_copy(
        &config.provider,
        config.source_language.as_deref(),
        &config.target_language,
    );
    let blocks = parse_and_validate(&response.content, segment, mode, validate_source_copy)?;

    Ok(SegmentTranslation {
        segment_id: segment.id.clone(),
        ordinal: segment.ordinal,
        block_ids: segment.block_ids.clone(),
        blocks,
        checksum: segment.checksum.clone(),
        status: SegmentStatus::Succeeded,
        template: template.name.clone(),
        error: None,
        input_tokens: response.input_tokens,
        input_cached_tokens: response.input_cached_tokens,
        output_tokens: response.output_tokens,
        tokens_estimated: false,
    })
}

fn max_output_tokens(
    segment: &Segment,
    mode: TranslationMode,
    reasoning: bool,
    target_language: &str,
    extended_output: bool,
) -> u32 {
    let source_tokens = segment.source.token_estimate.max(1);
    let block_overhead = segment.source.blocks.len().saturating_mul(128);
    let marker_overhead = match mode {
        TranslationMode::Plain => 128,
        TranslationMode::MarkerSafe => segment
            .constraints
            .preserve_markers
            .len()
            .saturating_mul(24),
        TranslationMode::RunPreserving => segment
            .source
            .blocks
            .iter()
            .map(|block| block.text_runs.len())
            .sum::<usize>()
            .saturating_mul(32),
    };
    let built_in_policy = bookforge_core::style::built_in_sizing_policy_for_target(target_language);
    let built_in_multiplier = built_in_policy
        .map(|policy| policy.output_token_multiplier)
        .unwrap_or(0);
    let built_in_minimum = built_in_policy
        .map(|policy| policy.min_output_tokens as usize)
        .unwrap_or(512);
    let source_multiplier: usize = (if reasoning { 8 } else { 3 }).max(built_in_multiplier);
    let max_cap: usize = if reasoning || extended_output {
        32_768
    } else if built_in_multiplier > 0 {
        16_384
    } else {
        8_192
    };
    let estimate = source_tokens
        .saturating_mul(source_multiplier)
        .saturating_add(block_overhead)
        .saturating_add(marker_overhead)
        .max(512);
    estimate.max(built_in_minimum).min(max_cap) as u32
}

fn render_prompt(
    template: &PromptTemplate,
    segment: &Segment,
    config: &TranslationRunConfig,
    mode: TranslationMode,
    context_pairs: &[CompletedContext],
) -> Result<crate::prompt::Rendered> {
    let (glossary_json, glossary_prose) = glossary_for_segment(config, &segment.id.0);
    let context_block = render_context_pairs(context_pairs);
    let mut vars = Substitutions::new();
    vars.string(
        "source_language",
        config.source_language.as_deref().unwrap_or("auto"),
    )
    .string("target_language", &config.target_language)
    .string("segment_id", &segment.id.0)
    .string(
        "book_title",
        segment.metadata.book_title.as_deref().unwrap_or(""),
    )
    .string(
        "section_title",
        segment.metadata.section_title.as_deref().unwrap_or(""),
    )
    .number("section_index", segment.metadata.section_index)
    .number("segment_index", segment.metadata.segment_index_in_section)
    .number(
        "total_segments_in_section",
        segment.metadata.total_segments_in_section.max(1),
    )
    .raw(
        "context_before",
        segment.context.before.clone().unwrap_or_default(),
    )
    .raw(
        "context_after",
        segment.context.after.clone().unwrap_or_default(),
    )
    .json("glossary_json", &glossary_json)
    .raw("glossary_block_prose", glossary_prose)
    .raw("context_translation_pairs", context_block)
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
    .raw(
        "prompt_extra",
        prompt_extra_for_segment(config, &segment.id.0),
    )
    .json(
        "protected_spans_json",
        &Value::Array(
            segment
                .constraints
                .preserve_spans
                .iter()
                .map(|span| Value::String(span.clone()))
                .collect(),
        ),
    );

    match mode {
        TranslationMode::Plain => {
            vars.raw("source_text", &segment.source.text);
        }
        TranslationMode::MarkerSafe => {
            vars.json(
                "source_blocks_json",
                &Value::Array(
                    segment
                        .source
                        .blocks
                        .iter()
                        .map(segment_block_to_json)
                        .collect(),
                ),
            )
            .json(
                "required_markers_json",
                &Value::Array(
                    segment
                        .constraints
                        .preserve_markers
                        .iter()
                        .map(|marker| Value::String(marker.clone()))
                        .collect(),
                ),
            );
        }
        TranslationMode::RunPreserving => {
            vars.json(
                "source_run_blocks_json",
                &Value::Array(
                    segment
                        .source
                        .blocks
                        .iter()
                        .map(segment_run_block_to_json)
                        .collect(),
                ),
            );
        }
    }

    template
        .render(&vars)
        .map_err(|err| LlmError::Provider(format!("prompt render failed: {err}")))
}

pub(crate) fn prompt_extra_for_segment(config: &TranslationRunConfig, segment_id: &str) -> String {
    match (
        config.glossary.prompt_extra.as_deref(),
        config.glossary.guidance_by_segment.get(segment_id),
    ) {
        (Some(global), Some(guidance)) if !global.trim().is_empty() => {
            format!("{global}\n\nRetry guidance for this segment:\n{guidance}")
        }
        (_, Some(guidance)) => format!("Retry guidance for this segment:\n{guidance}"),
        (Some(global), None) => global.to_string(),
        (None, None) => String::new(),
    }
}

pub(crate) fn render_context_pairs(pairs: &[CompletedContext]) -> String {
    if pairs.is_empty() {
        return String::new();
    }
    // Pairs arrive closest-first; chronological order in the prompt reads
    // better, so we flip them for rendering.
    let mut out = String::from("=== Context (already translated, do not retranslate) ===\n");
    let mut first = true;
    for ctx in pairs.iter().rev() {
        if !first {
            out.push_str("---\n");
        }
        out.push_str("Source: ");
        out.push_str(ctx.source_text.trim());
        out.push('\n');
        out.push_str("Target: ");
        out.push_str(ctx.translated_text.trim());
        out.push('\n');
        first = false;
    }
    out.push_str("=== End context ===\n");
    out
}

pub(crate) fn glossary_for_segment(
    config: &TranslationRunConfig,
    segment_id: &str,
) -> (Value, String) {
    let entries = config
        .glossary
        .entries_by_segment
        .get(segment_id)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    match config.glossary.format {
        GlossaryFormat::Json => (
            serde_json::to_value(entries).unwrap_or(Value::Array(vec![])),
            String::new(),
        ),
        GlossaryFormat::Prose => (Value::Array(vec![]), render_glossary_prose(entries)),
    }
}

pub(crate) fn render_glossary_prose(entries: &[GlossaryPromptTerm]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let mut out = String::from("Active glossary constraints (must be honored):\n");
    for entry in entries {
        out.push_str("- \"");
        out.push_str(&entry.source);
        out.push_str("\" -> \"");
        out.push_str(&entry.target);
        out.push_str("\" (");
        out.push_str(entry.category.as_str());
        out.push(')');
        if let Some(note) = entry.note.as_deref().filter(|note| !note.is_empty()) {
            out.push_str(": ");
            out.push_str(note);
        }
        out.push('\n');
    }
    out
}

fn segment_block_to_json(block: &SegmentBlock) -> Value {
    json!({
        "block_id": block.block_id.0,
        "kind": block.kind,
        "text": block.text,
    })
}

fn segment_run_block_to_json(block: &SegmentBlock) -> Value {
    json!({
        "block_id": block.block_id.0,
        "kind": block.kind,
        "runs": block.text_runs
            .iter()
            .map(|run| json!({
                "id": run.id,
                "text": run.text,
            }))
            .collect::<Vec<_>>(),
        "protected_spans": block
            .protected_spans
            .iter()
            .map(|span| span.text.as_str())
            .collect::<Vec<_>>(),
    })
}

fn parse_and_validate(
    content: &str,
    segment: &Segment,
    mode: TranslationMode,
    validate_source_copy: bool,
) -> Result<Vec<BlockTranslation>> {
    match mode {
        TranslationMode::Plain => {
            let parsed: PlainTranslationResponse = serde_json::from_str(content)?;
            if parsed.segment_id != segment.id.0 {
                return Err(LlmError::InvalidResponse(format!(
                    "segment id mismatch: expected '{}', got '{}'",
                    segment.id.0, parsed.segment_id
                )));
            }
            let expected_spans: &[ProtectedSpan] = segment
                .source
                .blocks
                .first()
                .map(|block| block.protected_spans.as_slice())
                .unwrap_or(&[]);
            let translation = parsed.translation;
            if let Some(error) = crate::validation::empty_translation_validation_error(
                &segment.source.text,
                &translation,
            ) {
                return Err(LlmError::InvalidResponse(error.to_string()));
            }
            validate_protected_spans(segment, expected_spans, &translation)?;
            if validate_source_copy
                && let Some(error) = crate::validation::source_copy_validation_error(
                    &segment.source.text,
                    &translation,
                    segment.metadata.section_title.as_deref(),
                )
            {
                return Err(LlmError::InvalidResponse(error));
            }
            let block_id = segment
                .block_ids
                .first()
                .cloned()
                .unwrap_or_else(|| BlockId(format!("{}_block", segment.id.0)));
            Ok(vec![BlockTranslation {
                block_id,
                text: translation,
            }])
        }
        TranslationMode::MarkerSafe => {
            let parsed: MarkerSafeTranslationResponse = serde_json::from_str(content)?;
            if parsed.segment_id != segment.id.0 {
                return Err(LlmError::InvalidResponse(format!(
                    "segment id mismatch: expected '{}', got '{}'",
                    segment.id.0, parsed.segment_id
                )));
            }

            let expected_block_ids: Vec<&str> =
                segment.block_ids.iter().map(|id| id.0.as_str()).collect();
            if parsed.blocks.len() != expected_block_ids.len() {
                return Err(LlmError::InvalidResponse(format!(
                    "segment '{}' expected {} block translations, got {}",
                    segment.id.0,
                    expected_block_ids.len(),
                    parsed.blocks.len()
                )));
            }

            let mut by_id = std::collections::HashMap::with_capacity(parsed.blocks.len());
            for block in parsed.blocks {
                if by_id
                    .insert(block.block_id.clone(), block.translation)
                    .is_some()
                {
                    return Err(LlmError::InvalidResponse(format!(
                        "segment '{}' returned duplicate block_id '{}'",
                        segment.id.0, block.block_id
                    )));
                }
            }

            let mut translations = Vec::with_capacity(expected_block_ids.len());
            for source_block in &segment.source.blocks {
                let translation =
                    by_id
                        .remove(source_block.block_id.0.as_str())
                        .ok_or_else(|| {
                            LlmError::InvalidResponse(format!(
                                "segment '{}' is missing translation for block '{}'",
                                segment.id.0, source_block.block_id.0
                            ))
                        })?;
                if let Some(error) = crate::validation::empty_translation_validation_error(
                    &source_block.text,
                    &translation,
                ) {
                    return Err(LlmError::InvalidResponse(format!(
                        "segment '{}' block '{}': {error}",
                        segment.id.0, source_block.block_id.0
                    )));
                }
                let expected_markers = marker_ids_in_text(&source_block.text);
                validate_markers(segment, &expected_markers, &translation)?;
                validate_protected_spans(segment, &source_block.protected_spans, &translation)?;
                if validate_source_copy
                    && let Some(error) = crate::validation::source_copy_validation_error(
                        &source_block.text,
                        &translation,
                        segment.metadata.section_title.as_deref(),
                    )
                {
                    return Err(LlmError::InvalidResponse(format!(
                        "segment '{}' block '{}': {error}",
                        segment.id.0, source_block.block_id.0
                    )));
                }
                translations.push(BlockTranslation {
                    block_id: source_block.block_id.clone(),
                    text: translation,
                });
            }

            if !by_id.is_empty() {
                let extras: Vec<String> = by_id.into_keys().collect();
                return Err(LlmError::InvalidResponse(format!(
                    "segment '{}' returned unexpected block ids: {}",
                    segment.id.0,
                    extras.join(", ")
                )));
            }

            Ok(translations)
        }
        TranslationMode::RunPreserving => {
            let parsed: RunPreservingTranslationResponse = serde_json::from_str(content)?;
            if parsed.segment_id != segment.id.0 {
                return Err(LlmError::InvalidResponse(format!(
                    "segment id mismatch: expected '{}', got '{}'",
                    segment.id.0, parsed.segment_id
                )));
            }

            let mut by_id = std::collections::HashMap::with_capacity(parsed.blocks.len());
            for block in parsed.blocks {
                let block_id = block.block_id.clone();
                if by_id.insert(block_id.clone(), block).is_some() {
                    return Err(LlmError::InvalidResponse(format!(
                        "segment '{}' returned duplicate block_id '{}'",
                        segment.id.0, block_id
                    )));
                }
            }

            let mut translations = Vec::with_capacity(segment.source.blocks.len());
            for source_block in &segment.source.blocks {
                let block = by_id
                    .remove(source_block.block_id.0.as_str())
                    .ok_or_else(|| {
                        LlmError::InvalidResponse(format!(
                            "segment '{}' is missing run-preserving translation for block '{}'",
                            segment.id.0, source_block.block_id.0
                        ))
                    })?;
                let text = validate_and_join_runs(segment, source_block, block.translated_runs)?;
                if let Some(error) =
                    crate::validation::empty_translation_validation_error(&source_block.text, &text)
                {
                    return Err(LlmError::InvalidResponse(format!(
                        "segment '{}' block '{}': {error}",
                        segment.id.0, source_block.block_id.0
                    )));
                }
                let expected_markers = marker_ids_in_text(&source_block.text);
                validate_markers(segment, &expected_markers, &text)?;
                validate_protected_spans(segment, &source_block.protected_spans, &text)?;
                if validate_source_copy
                    && let Some(error) = crate::validation::source_copy_validation_error(
                        &source_block.text,
                        &text,
                        segment.metadata.section_title.as_deref(),
                    )
                {
                    return Err(LlmError::InvalidResponse(format!(
                        "segment '{}' block '{}': {error}",
                        segment.id.0, source_block.block_id.0
                    )));
                }
                translations.push(BlockTranslation {
                    block_id: source_block.block_id.clone(),
                    text,
                });
            }

            if !by_id.is_empty() {
                let extras: Vec<String> = by_id.into_keys().collect();
                return Err(LlmError::InvalidResponse(format!(
                    "segment '{}' returned unexpected block ids: {}",
                    segment.id.0,
                    extras.join(", ")
                )));
            }

            Ok(translations)
        }
    }
}

fn validate_and_join_runs(
    segment: &Segment,
    source_block: &SegmentBlock,
    translated_runs: Vec<TranslatedRun>,
) -> Result<String> {
    let expected_ids = source_block
        .text_runs
        .iter()
        .map(|run| run.id.as_str())
        .collect::<Vec<_>>();
    if translated_runs.len() != expected_ids.len() {
        return Err(LlmError::InvalidResponse(format!(
            "segment '{}' block '{}' expected {} translated runs, got {}",
            segment.id.0,
            source_block.block_id.0,
            expected_ids.len(),
            translated_runs.len()
        )));
    }

    let mut by_id = std::collections::HashMap::with_capacity(translated_runs.len());
    for run in translated_runs {
        if by_id.insert(run.id.clone(), run.text).is_some() {
            return Err(LlmError::InvalidResponse(format!(
                "segment '{}' block '{}' returned duplicate run id '{}'",
                segment.id.0, source_block.block_id.0, run.id
            )));
        }
    }

    let mut text = String::new();
    for run in &source_block.text_runs {
        let value = by_id.remove(run.id.as_str()).ok_or_else(|| {
            LlmError::InvalidResponse(format!(
                "segment '{}' block '{}' is missing translated run '{}'",
                segment.id.0, source_block.block_id.0, run.id
            ))
        })?;
        if is_marker_token(&run.text) && value != run.text {
            return Err(LlmError::InvalidResponse(format!(
                "segment '{}' block '{}' changed marker run '{}'",
                segment.id.0, source_block.block_id.0, run.id
            )));
        }
        text.push_str(&value);
    }

    if !by_id.is_empty() {
        let extras = by_id.into_keys().collect::<Vec<_>>();
        return Err(LlmError::InvalidResponse(format!(
            "segment '{}' block '{}' returned unexpected run ids: {}",
            segment.id.0,
            source_block.block_id.0,
            extras.join(", ")
        )));
    }

    Ok(text)
}

fn is_validator_error(error: &LlmError) -> bool {
    matches!(error, LlmError::InvalidResponse(_) | LlmError::Json(_))
}

fn has_structured_runs(segment: &Segment) -> bool {
    segment.source.blocks.iter().any(|block| {
        block.text_runs.len() > 1 || block.text_runs.iter().any(|run| is_marker_token(&run.text))
    })
}

fn validation_retry_appendix(segment: &Segment, mode: TranslationMode, error: &str) -> String {
    let markers = segment
        .constraints
        .preserve_markers
        .iter()
        .map(|marker| format!("- {marker}"))
        .collect::<Vec<_>>()
        .join("\n");
    let markers = if markers.is_empty() {
        "- none".to_string()
    } else {
        markers
    };
    format!(
        "\n\nValidation failed on the previous attempt.\n\
         Error:\n{error}\n\n\
         Retry instructions:\n\
         - Return only valid JSON for `{}`.\n\
         - Preserve every required marker exactly once.\n\
         - Do not add unknown markers.\n\
         - Do not change protected spans.\n\n\
         Required markers:\n{markers}\n",
        mode.template_name()
    )
}

fn validate_protected_spans(
    segment: &Segment,
    spans: &[ProtectedSpan],
    translation: &str,
) -> Result<()> {
    for span in spans {
        if !crate::validation::protected_span_present(&span.text, translation) {
            return Err(LlmError::InvalidResponse(format!(
                "protected span missing from segment '{}': {}",
                segment.id.0, span.text
            )));
        }
    }
    Ok(())
}

fn validate_markers(segment: &Segment, expected: &[String], translation: &str) -> Result<()> {
    if let Some(error) = bookforge_core::marker::marker_structure_error(translation) {
        return Err(LlmError::InvalidResponse(format!(
            "invalid inline marker structure in segment '{}': {error}",
            segment.id.0
        )));
    }
    let actual = marker_ids_in_text(translation);

    for marker in expected {
        let count = actual.iter().filter(|actual| *actual == marker).count();
        if count == 0 {
            return Err(LlmError::InvalidResponse(format!(
                "inline marker missing from segment '{}': {}",
                segment.id.0, marker
            )));
        }
        if count > 1 {
            return Err(LlmError::InvalidResponse(format!(
                "inline marker duplicated in segment '{}': {}",
                segment.id.0, marker
            )));
        }
    }

    for marker in &actual {
        if !expected.iter().any(|expected| expected == marker) {
            return Err(LlmError::InvalidResponse(format!(
                "unknown inline marker in segment '{}': {}",
                segment.id.0, marker
            )));
        }
    }

    Ok(())
}

use bookforge_core::marker::{is_marker_token, marker_ids_in_text};

fn needs_review_translation_with_tokens(
    segment: &Segment,
    mode: TranslationMode,
    error: String,
    input_tokens: Option<u64>,
    input_cached_tokens: Option<u64>,
    output_tokens: Option<u64>,
) -> SegmentTranslation {
    SegmentTranslation {
        segment_id: segment.id.clone(),
        ordinal: segment.ordinal,
        block_ids: segment.block_ids.clone(),
        blocks: source_fallback_blocks(segment),
        checksum: segment.checksum.clone(),
        status: SegmentStatus::NeedsReview,
        template: mode.template_name().to_string(),
        error: Some(error),
        input_tokens,
        input_cached_tokens,
        output_tokens,
        tokens_estimated: false,
    }
}

fn failed_translation_with_tokens(
    segment: &Segment,
    mode: TranslationMode,
    error: String,
    input_tokens: Option<u64>,
    input_cached_tokens: Option<u64>,
    output_tokens: Option<u64>,
) -> SegmentTranslation {
    SegmentTranslation {
        segment_id: segment.id.clone(),
        ordinal: segment.ordinal,
        block_ids: segment.block_ids.clone(),
        blocks: source_fallback_blocks(segment),
        checksum: segment.checksum.clone(),
        status: SegmentStatus::Failed,
        template: mode.template_name().to_string(),
        error: Some(error),
        input_tokens,
        input_cached_tokens,
        output_tokens,
        tokens_estimated: false,
    }
}

fn source_fallback_blocks(segment: &Segment) -> Vec<BlockTranslation> {
    if segment.source.blocks.is_empty() {
        return segment
            .block_ids
            .iter()
            .map(|block_id| BlockTranslation {
                block_id: block_id.clone(),
                text: segment.source.text.clone(),
            })
            .collect();
    }
    segment
        .source
        .blocks
        .iter()
        .map(|block| BlockTranslation {
            block_id: block.block_id.clone(),
            text: block.text.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use std::time::Duration;

    use bookforge_core::{
        ir::{ProtectedSpan, ProtectedSpanKind, SectionId},
        segment::{
            Segment, SegmentBlock, SegmentConstraints, SegmentContext, SegmentMetadata,
            SegmentSource, SegmentTextRun,
        },
    };

    use super::*;
    use crate::provider::{
        CompletionRequest, CompletionResponse, MockMode, MockProvider, ProviderCapabilities,
    };

    #[tokio::test]
    async fn single_block_segment_uses_plain_mode_and_returns_translation() {
        let segments = vec![segment("seg_a", 0, vec![("b0", "First")])];

        let translations = translate_segments(
            MockProvider::new(MockMode::PrefixTarget, "Italian"),
            &segments,
            &config(),
        )
        .await
        .expect("mock translation should succeed");

        assert_eq!(translations.len(), 1);
        assert_eq!(translations[0].status, SegmentStatus::Succeeded);
        assert_eq!(translations[0].template, "translate_segment");
        assert_eq!(translations[0].blocks.len(), 1);
        assert_eq!(translations[0].blocks[0].text, "[Italian] First");
    }

    #[tokio::test]
    async fn multi_block_segment_uses_marker_safe_and_returns_per_block() {
        let segments = vec![segment(
            "seg_a",
            0,
            vec![("b0", "First paragraph."), ("b1", "Second paragraph.")],
        )];

        let translations = translate_segments(
            MockProvider::new(MockMode::PrefixTarget, "Italian"),
            &segments,
            &config(),
        )
        .await
        .expect("mock translation should succeed");

        assert_eq!(translations[0].template, "translate_marker_safe");
        assert_eq!(translations[0].blocks.len(), 2);
        assert_eq!(translations[0].blocks[0].block_id.0, "b0");
        assert_eq!(translations[0].blocks[0].text, "[Italian] First paragraph.");
        assert_eq!(translations[0].blocks[1].block_id.0, "b1");
        assert_eq!(
            translations[0].blocks[1].text,
            "[Italian] Second paragraph."
        );
    }

    #[test]
    fn non_batch_parser_rejects_copied_source_prose() {
        let source = "This deliberately long English paragraph contains enough ordinary prose to \
            exercise untranslated-copy detection in a non-batch response. The provider returned \
            the entire source paragraph unchanged instead of translating it into the requested \
            target language, so validation must reject the response before checkpointing it.";
        let segment = segment("seg_a", 0, vec![("b0", source)]);
        let response = serde_json::json!({
            "segment_id": "seg_a",
            "translation": source,
        })
        .to_string();

        let error = parse_and_validate(&response, &segment, TranslationMode::Plain, true)
            .expect_err("long unchanged source prose should fail");

        assert!(error.to_string().contains("source-language prose"));
    }

    #[test]
    fn non_batch_plain_rejects_whitespace_translation_for_non_empty_source() {
        let segment = segment("seg_a", 0, vec![("b0", "Visible prose")]);
        let response = serde_json::json!({
            "segment_id": "seg_a",
            "translation": " \n\t",
        })
        .to_string();

        let error = parse_and_validate(&response, &segment, TranslationMode::Plain, false)
            .expect_err("whitespace-only translation must fail");

        assert!(
            error
                .to_string()
                .contains("empty translation for non-empty source")
        );
    }

    #[test]
    fn non_batch_marker_safe_rejects_empty_translation_for_non_empty_source() {
        let segment = segment(
            "seg_a",
            0,
            vec![("b0", "First paragraph."), ("b1", "Second paragraph.")],
        );
        let response = serde_json::json!({
            "segment_id": "seg_a",
            "blocks": [
                {"block_id": "b0", "translation": "Primo paragrafo."},
                {"block_id": "b1", "translation": ""},
            ],
        })
        .to_string();

        let error = parse_and_validate(&response, &segment, TranslationMode::MarkerSafe, false)
            .expect_err("empty non-empty block translation must fail");

        assert!(
            error
                .to_string()
                .contains("empty translation for non-empty source")
        );
    }

    #[test]
    fn non_batch_marker_safe_allows_genuinely_empty_source_block() {
        let segment = segment("seg_a", 0, vec![("b0", ""), ("b1", "Second paragraph.")]);
        let response = serde_json::json!({
            "segment_id": "seg_a",
            "blocks": [
                {"block_id": "b0", "translation": " \n"},
                {"block_id": "b1", "translation": "Secondo paragrafo."},
            ],
        })
        .to_string();

        let blocks = parse_and_validate(&response, &segment, TranslationMode::MarkerSafe, false)
            .expect("an empty source block may have an empty translation");

        assert_eq!(blocks[0].text, " \n");
        assert_eq!(blocks[1].text, "Secondo paragrafo.");
    }

    #[tokio::test]
    async fn empty_plain_translation_never_returns_cache_eligible_success() {
        let segments = vec![segment("seg_a", 0, vec![("b0", "Visible prose")])];

        let translations = translate_segments(WhitespaceTranslationProvider, &segments, &config())
            .await
            .expect("validation failure should become NeedsReview");

        assert_eq!(translations.len(), 1);
        assert_eq!(translations[0].status, SegmentStatus::NeedsReview);
        assert!(
            translations[0]
                .error
                .as_deref()
                .is_some_and(|error| error.contains("empty translation for non-empty source"))
        );
        assert_eq!(translations[0].blocks[0].text, "Visible prose");
    }

    #[tokio::test]
    async fn parallel_segments_complete_in_order() {
        let segments = vec![
            segment("seg_b", 1, vec![("b0", "Second")]),
            segment("seg_a", 0, vec![("b0", "First")]),
        ];

        let translations = translate_segments(
            MockProvider::new(MockMode::PrefixTarget, "Italian"),
            &segments,
            &config(),
        )
        .await
        .expect("mock translation should succeed");

        assert_eq!(translations[0].segment_id.0, "seg_a");
        assert_eq!(translations[1].segment_id.0, "seg_b");
    }

    #[tokio::test]
    async fn live_concurrency_shrink_bounds_later_segment_dispatch() {
        let segments = (0..4)
            .map(|ordinal| segment(&format!("seg_{ordinal}"), ordinal, vec![("b0", "Text")]))
            .collect::<Vec<_>>();
        let provider = GatedProvider::new();
        let mut cfg = config();
        cfg.scheduler.concurrency = 2;
        let mut runtime = EngineRuntimeSettings {
            revision: 1,
            batch: TranslationProfile::V1Fast.resolve().batch,
            batch_max_output_tokens: None,
            concurrency: 2,
            provider_max_attempts: 1,
            adaptive_concurrency: false,
        };
        let (sender, receiver) = tokio::sync::watch::channel(runtime.clone());
        cfg.runtime_settings = Some(receiver);

        let run_provider = provider.clone();
        let run = tokio::spawn(async move {
            translate_segments(run_provider, &segments, &cfg)
                .await
                .expect("translation should finish")
        });

        wait_for_request_count(&provider.started, 2).await;
        runtime.revision = 2;
        runtime.concurrency = 1;
        sender.send_replace(runtime);
        provider.release.add_permits(2);

        wait_for_request_count(&provider.started, 3).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            provider.started.load(Ordering::Acquire),
            3,
            "after shrinking to one, the fourth request must wait for the third"
        );

        provider.release.add_permits(1);
        wait_for_request_count(&provider.started, 4).await;
        provider.release.add_permits(1);
        let translations = tokio::time::timeout(Duration::from_secs(2), run)
            .await
            .expect("run should finish")
            .expect("task should join");
        assert_eq!(translations.len(), 4);
    }

    #[tokio::test]
    async fn pause_reconfigure_resume_dispatches_only_the_new_revision() {
        let segments = (0..2)
            .map(|ordinal| segment(&format!("seg_{ordinal}"), ordinal, vec![("b0", "Text")]))
            .collect::<Vec<_>>();
        let provider = GatedProvider::new();
        let signal = PauseSignal::new();
        signal.pause();
        let mut cfg = config();
        cfg.scheduler.concurrency = 1;
        cfg.pause_signal = Some(signal.clone());
        let mut runtime = EngineRuntimeSettings {
            revision: 1,
            batch: TranslationProfile::V1Fast.resolve().batch,
            batch_max_output_tokens: Some(512),
            concurrency: 1,
            provider_max_attempts: 1,
            adaptive_concurrency: false,
        };
        let (sender, receiver) = tokio::sync::watch::channel(runtime.clone());
        cfg.runtime_settings = Some(receiver);

        let run_provider = provider.clone();
        let run = tokio::spawn(async move {
            translate_segments(run_provider, &segments, &cfg)
                .await
                .expect("translation should finish")
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(provider.started.load(Ordering::Acquire), 0);

        runtime.revision = 2;
        runtime.provider_max_attempts = 4;
        sender.send_replace(runtime);
        signal.resume();

        wait_for_request_count(&provider.started, 1).await;
        provider.release.add_permits(1);
        wait_for_request_count(&provider.started, 2).await;
        provider.release.add_permits(1);
        let translations = tokio::time::timeout(Duration::from_secs(2), run)
            .await
            .expect("run should finish")
            .expect("task should join");
        assert_eq!(translations.len(), 2);
        let metadata = provider.metadata.lock().expect("metadata log mutex");
        assert_eq!(metadata.len(), 2);
        assert!(metadata.iter().all(|entry| {
            entry.runtime_config_revision == Some(2) && entry.provider_max_attempts == Some(4)
        }));
    }

    #[tokio::test]
    async fn closed_runtime_channel_retains_its_last_snapshot() {
        let mut cfg = config();
        let runtime = EngineRuntimeSettings {
            revision: 5,
            batch: TranslationProfile::V1Fast.resolve().batch,
            batch_max_output_tokens: Some(768),
            concurrency: 1,
            provider_max_attempts: 3,
            adaptive_concurrency: false,
        };
        let (sender, receiver) = tokio::sync::watch::channel(runtime);
        cfg.runtime_settings = Some(receiver);
        drop(sender);

        assert_eq!(cfg.request_runtime_metadata(), (Some(5), Some(3)));
        let translations = translate_segments(
            MockProvider::new(MockMode::PrefixTarget, "Italian"),
            &[segment("seg_a", 0, vec![("b0", "First")])],
            &cfg,
        )
        .await
        .expect("a closed sender must not fail an active run");
        assert_eq!(translations.len(), 1);
        assert_eq!(translations[0].status, SegmentStatus::Succeeded);
    }

    #[tokio::test]
    async fn stop_wins_over_a_runtime_revision_before_dispatch() {
        let provider = GatedProvider::new();
        let signal = PauseSignal::new();
        signal.stop();
        let mut cfg = config();
        cfg.pause_signal = Some(signal);
        let mut runtime = EngineRuntimeSettings {
            revision: 1,
            batch: TranslationProfile::V1Fast.resolve().batch,
            batch_max_output_tokens: None,
            concurrency: 2,
            provider_max_attempts: 1,
            adaptive_concurrency: false,
        };
        let (sender, receiver) = tokio::sync::watch::channel(runtime.clone());
        cfg.runtime_settings = Some(receiver);
        runtime.revision = 2;
        runtime.concurrency = 4;
        sender.send_replace(runtime);

        let translations = translate_segments(
            provider.clone(),
            &[segment("seg_a", 0, vec![("b0", "First")])],
            &cfg,
        )
        .await
        .expect("stop should terminate dispatch cleanly");
        assert!(translations.is_empty());
        assert_eq!(provider.started.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn non_batch_context_waiters_do_not_consume_scheduler_permits() {
        let segments = vec![
            segment("seg_d", 3, vec![("b0", "Fourth")]),
            segment("seg_c", 2, vec![("b0", "Third")]),
            segment("seg_b", 1, vec![("b0", "Second")]),
            segment("seg_a", 0, vec![("b0", "First")]),
        ];
        let mut cfg = config();
        cfg.scheduler.concurrency = 1;
        cfg.context = ContextRunConfig {
            window: 1,
            budget_tokens: 1000,
            scope: ContextScope::Book,
            strict: true,
        };
        cfg.context_registry = Some(Arc::new(ContextRegistry::new(&segments)));

        let run = translate_segments(
            MockProvider::new(MockMode::PrefixTarget, "Italian"),
            &segments,
            &cfg,
        );
        let translations = tokio::time::timeout(std::time::Duration::from_secs(5), run)
            .await
            .expect("context waiters must not starve predecessor segments")
            .expect("mock translation should succeed");

        assert_eq!(
            translations
                .iter()
                .map(|translation| translation.segment_id.0.as_str())
                .collect::<Vec<_>>(),
            vec!["seg_a", "seg_b", "seg_c", "seg_d"]
        );
        assert!(
            translations
                .iter()
                .all(|translation| translation.status == SegmentStatus::Succeeded)
        );
    }

    #[tokio::test]
    async fn validator_failure_is_marked_needs_review() {
        let segments = vec![segment("seg_a", 0, vec![("b0", "First")])];

        let translations = translate_segments(
            MockProvider::new(MockMode::WrongSegmentId, "Italian"),
            &segments,
            &config(),
        )
        .await
        .expect("validator failure should not propagate");

        assert_eq!(translations[0].status, SegmentStatus::NeedsReview);
        assert_eq!(translations[0].blocks[0].text, "First");
        assert!(
            translations[0]
                .error
                .as_deref()
                .is_some_and(|err| err.contains("segment id mismatch"))
        );
    }

    #[tokio::test]
    async fn protected_span_failure_surfaces_as_needs_review() {
        // The uppercase mock destroys the URL's casing on every attempt;
        // that is a genuine translation defect and must be flagged for
        // review with the source preserved, not papered over by appending
        // the original URL after the mangled one.
        let mut segment = segment("seg_a", 0, vec![("b0", "Visit https://example.com")]);
        segment.source.blocks[0]
            .protected_spans
            .push(protected_span(
                ProtectedSpanKind::Url,
                "https://example.com",
            ));
        segment
            .constraints
            .preserve_spans
            .push("https://example.com".to_string());

        let translations = translate_segments(
            MockProvider::new(MockMode::Uppercase, "Italian"),
            &[segment],
            &config(),
        )
        .await
        .expect("validation failure should not propagate");

        assert_eq!(translations[0].status, SegmentStatus::NeedsReview);
        assert!(
            translations[0]
                .error
                .as_deref()
                .is_some_and(|error| error.contains("protected span missing")),
            "error should name the missing span, got: {:?}",
            translations[0].error
        );
        assert_eq!(
            translations[0].blocks[0].text, "Visit https://example.com",
            "needs-review fallback must preserve the source text"
        );
    }

    #[tokio::test]
    async fn missing_protected_span_fails_validation_instead_of_appending() {
        let mut segment = segment("seg_a", 0, vec![("b0", "The 4th day")]);
        segment.source.blocks[0]
            .protected_spans
            .push(protected_span(ProtectedSpanKind::Number, "4th"));
        segment.constraints.preserve_spans.push("4th".to_string());

        let translations = translate_segments(MissingProtectedSpanProvider, &[segment], &config())
            .await
            .expect("validation failure should not propagate");

        // A dropped protected span must surface as needs-review with the
        // source text preserved — never be silently glued onto the end of
        // the translated paragraph.
        assert_eq!(translations[0].status, SegmentStatus::NeedsReview);
        assert!(
            translations[0]
                .error
                .as_deref()
                .is_some_and(|error| error.contains("protected span missing")),
            "error should name the missing span, got: {:?}",
            translations[0].error
        );
        assert_eq!(
            translations[0].blocks[0].text, "The 4th day",
            "needs-review fallback must preserve the source text"
        );
    }

    #[test]
    fn localized_decimal_protected_span_passes_validation() {
        let segment = segment("seg_a", 0, vec![("b0", "diameter 0.1 to 1 mm")]);

        validate_protected_spans(
            &segment,
            &[protected_span(ProtectedSpanKind::Number, "0.1")],
            "diametro da 0,1 a 1 mm",
        )
        .expect("decimal comma localization should preserve numeric value");
    }

    #[test]
    fn localized_negative_decimal_protected_span_passes_validation() {
        let segment = segment("seg_a", 0, vec![("b0", "potential -63.5 mV")]);

        validate_protected_spans(
            &segment,
            &[protected_span(ProtectedSpanKind::Number, "-63.5")],
            "il potenziale era circa –63,5 mV",
        )
        .expect("localized minus and decimal comma should preserve numeric value");
    }

    #[test]
    fn citation_spacing_protected_span_passes_validation() {
        let segment = segment("seg_a", 0, vec![("b0", "Skou (1957,1989)")]);

        validate_protected_spans(
            &segment,
            &[protected_span(ProtectedSpanKind::Citation, "1957,1989")],
            "Skou (1957, 1989) isolò una ATPasi",
        )
        .expect("citation spacing should not count as a dropped numeric span");
    }

    #[test]
    fn dangling_numeric_protected_span_passes_validation() {
        let segment = segment("seg_a", 0, vec![("b0", "The value was 10-")]);

        validate_protected_spans(
            &segment,
            &[protected_span(ProtectedSpanKind::Number, "10-")],
            "7,3 × 10⁻⁷ mol cm⁻²",
        )
        .expect("dangling numeric extraction artifacts should not force review");
    }

    #[test]
    fn missing_numeric_protected_span_still_fails_validation() {
        let segment = segment("seg_a", 0, vec![("b0", "See section 5.16")]);

        let error = validate_protected_spans(
            &segment,
            &[protected_span(ProtectedSpanKind::Number, "5.16")],
            "Si noti che questa forma di rettificazione deriva dai canali aperti.",
        )
        .expect_err("absent numeric spans still need review");

        assert!(error.to_string().contains("protected span missing"));
    }

    #[tokio::test]
    async fn protected_span_only_required_in_block_that_contains_it() {
        // Block 0 has a span "1"; block 1 does not. The validator must not
        // require "1" to appear in block 1's translation.
        let mut segment = segment(
            "seg_a",
            0,
            vec![("b0", "Chapter 1"), ("b1", "Hello world.")],
        );
        segment.source.blocks[0]
            .protected_spans
            .push(protected_span(ProtectedSpanKind::Number, "1"));
        segment.constraints.preserve_spans.push("1".to_string());

        let translations = translate_segments(
            MockProvider::new(MockMode::PrefixTarget, "Italian"),
            &[segment],
            &config(),
        )
        .await
        .expect("scheduler should succeed");

        assert_eq!(translations[0].status, SegmentStatus::Succeeded);
        assert_eq!(translations[0].blocks[0].text, "[Italian] Chapter 1");
        assert_eq!(translations[0].blocks[1].text, "[Italian] Hello world.");
    }

    #[tokio::test]
    async fn provider_failure_is_segment_failed_not_run_error() {
        let segments = vec![segment("seg_a", 0, vec![("b0", "First")])];

        let translations = translate_segments(FailingProvider, &segments, &config())
            .await
            .expect("provider failure should be isolated to the segment");

        assert_eq!(translations.len(), 1);
        assert_eq!(translations[0].status, SegmentStatus::Failed);
        assert!(
            translations[0]
                .error
                .as_deref()
                .is_some_and(|error| error.contains("provider offline"))
        );
    }

    #[tokio::test]
    async fn pause_signal_gates_new_segment_dispatch() {
        let segments = vec![
            segment("seg_a", 0, vec![("b0", "First")]),
            segment("seg_b", 1, vec![("b0", "Second")]),
        ];
        let mut cfg = config();
        cfg.scheduler.concurrency = 1;
        let signal = PauseSignal::new();
        cfg.pause_signal = Some(signal.clone());
        let provider = BlockingProvider::new();
        let started = provider.started.clone();
        let release = provider.release.clone();
        let paused_once = Arc::new(AtomicBool::new(false));
        let pause_for_callback = paused_once.clone();
        let started_for_callback = started.clone();

        let run = tokio::spawn(async move {
            translate_segments_with_control(
                provider,
                &segments,
                &cfg,
                |_| Ok(()),
                None,
                |signal| {
                    if started_for_callback.load(Ordering::Acquire) >= 1
                        && !pause_for_callback.swap(true, Ordering::AcqRel)
                    {
                        signal.pause();
                    }
                    Ok(())
                },
            )
            .await
        });

        wait_for_request_count(&started, 1).await;
        release.notify_waiters();
        wait_for_pause(&signal).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            started.load(Ordering::Acquire),
            1,
            "second segment must not dispatch while paused"
        );

        signal.resume();
        let translations = tokio::time::timeout(Duration::from_secs(2), run)
            .await
            .expect("paused run should resume")
            .expect("scheduler task should not panic")
            .expect("scheduler should succeed");
        assert_eq!(translations.len(), 2);
        assert_eq!(started.load(Ordering::Acquire), 2);
    }

    #[tokio::test]
    async fn finalized_channel_close_is_run_error() {
        let segments = vec![segment("seg_a", 0, vec![("b0", "First")])];
        let (tx, rx) = mpsc::channel::<SegmentTranslation>(1);
        drop(rx);

        let result = translate_segments_with_callback(
            MockProvider::new(MockMode::PrefixTarget, "Italian"),
            &segments,
            &config(),
            |_| Ok(()),
            Some(tx),
        )
        .await;

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("finalized segment channel closed")
        );
    }

    #[tokio::test]
    async fn inline_marker_failure_is_marked_needs_review() {
        let mut segment = segment("seg_a", 0, vec![("b0", "Hello <m1>world</m1>!")]);
        segment.constraints.preserve_markers.push("m1".to_string());

        let translations = translate_segments(
            MockProvider::new(MockMode::Uppercase, "Italian"),
            &[segment],
            &config(),
        )
        .await
        .expect("marker validation failure should not propagate");

        assert_eq!(translations[0].status, SegmentStatus::NeedsReview);
        assert_eq!(translations[0].blocks[0].text, "Hello <m1>world</m1>!");
        assert!(
            translations[0]
                .error
                .as_deref()
                .is_some_and(|error| error.contains("inline marker missing"))
        );
    }

    #[tokio::test]
    async fn marker_failure_falls_back_to_run_preserving_mode() {
        let mut segment = segment("seg_a", 0, vec![("b0", "Hello <m1>world</m1>!")]);
        segment.source.blocks[0].text_runs = vec![
            SegmentTextRun {
                id: "r0".to_string(),
                text: "Hello ".to_string(),
            },
            SegmentTextRun {
                id: "r1".to_string(),
                text: "<m1>".to_string(),
            },
            SegmentTextRun {
                id: "r2".to_string(),
                text: "world".to_string(),
            },
            SegmentTextRun {
                id: "r3".to_string(),
                text: "</m1>".to_string(),
            },
            SegmentTextRun {
                id: "r4".to_string(),
                text: "!".to_string(),
            },
        ];
        segment.constraints.preserve_markers.push("m1".to_string());

        let mut config = config();
        config.scheduler.max_attempts = 1;
        let translations = translate_segments(
            MockProvider::new(MockMode::Uppercase, "Italian"),
            &[segment],
            &config,
        )
        .await
        .expect("run-preserving fallback should succeed");

        assert_eq!(translations[0].status, SegmentStatus::Succeeded);
        assert_eq!(translations[0].template, "translate_run_preserving");
        assert_eq!(translations[0].blocks[0].text, "HELLO <m1>WORLD</m1>!");
    }

    #[tokio::test]
    async fn malformed_json_response_is_marked_needs_review() {
        let segments = vec![segment("seg_a", 0, vec![("b0", "First")])];
        let translations = translate_segments(
            MockProvider::new(MockMode::MalformedJson, "Italian"),
            &segments,
            &config(),
        )
        .await
        .expect("malformed JSON should not propagate");

        assert_eq!(translations[0].status, SegmentStatus::NeedsReview);
        assert!(translations[0].error.is_some());
    }

    #[test]
    fn prompt_renders_glossary_json_prose_and_prompt_extra() {
        let segment = segment("seg_a", 0, vec![("b0", "Aragorn enters.")]);
        let mut json_config = config();
        json_config.glossary.entries_by_segment.insert(
            "seg_a".to_string(),
            vec![GlossaryPromptTerm {
                source: "Aragorn".to_string(),
                target: "Aragorn".to_string(),
                category: bookforge_core::GlossaryCategory::Person,
                note: Some("Preserve name".to_string()),
                term_id: Some(42),
                case_sensitive: true,
            }],
        );
        json_config.glossary.prompt_extra = Some("Maintain a literary register.".to_string());
        json_config.glossary.guidance_by_segment.insert(
            "seg_a".to_string(),
            "Keep the quoted phrase in Italian this time.".to_string(),
        );

        let rendered = render_prompt(
            &PromptLibrary::global().plain,
            &segment,
            &json_config,
            TranslationMode::Plain,
            &[],
        )
        .expect("prompt should render");
        assert!(rendered.user.contains("\"source\": \"Aragorn\""));
        assert!(rendered.user.contains("Maintain a literary register."));
        assert!(rendered.user.contains("Retry guidance for this segment"));
        assert!(
            rendered
                .user
                .contains("Keep the quoted phrase in Italian this time.")
        );

        let mut prose_config = json_config.clone();
        prose_config.glossary.format = bookforge_core::GlossaryFormat::Prose;
        let rendered = render_prompt(
            &PromptLibrary::global().plain,
            &segment,
            &prose_config,
            TranslationMode::Plain,
            &[],
        )
        .expect("prompt should render");
        assert!(rendered.user.contains("Active glossary constraints"));
        assert!(rendered.user.contains("\"Aragorn\" -> \"Aragorn\""));
    }

    #[test]
    fn run_preserving_prompt_projects_protected_spans_to_text() {
        let mut segment = segment("seg_a", 0, vec![("b0", "Chapter 42")]);
        segment.source.blocks[0].protected_spans =
            vec![protected_span(ProtectedSpanKind::Number, "42")];
        segment.constraints.preserve_spans = vec!["42".to_string()];

        let rendered = render_prompt(
            &PromptLibrary::global().run_preserving,
            &segment,
            &config(),
            TranslationMode::RunPreserving,
            &[],
        )
        .expect("run-preserving prompt should render");

        assert!(rendered.user.contains("42"));
        assert!(!rendered.user.contains("\"kind\": \"Number\""));
        assert!(!rendered.user.contains("\"kind\":\"Number\""));
    }

    fn config() -> TranslationRunConfig {
        TranslationRunConfig {
            source_language: Some("English".to_string()),
            target_language: "Italian".to_string(),
            provider: "mock".to_string(),
            model: "mock-prefix".to_string(),
            prompt_version: "v1".to_string(),
            temperature: 0.2,
            scheduler: SchedulerConfig {
                concurrency: 2,
                max_attempts: 1,
            },
            profile: TranslationProfile::Balanced,
            model_context_tokens: None,
            max_output_tokens: None,
            batch_max_output_tokens: None,
            compact_prompts: false,
            glossary: GlossaryRunConfig::default(),
            context: ContextRunConfig::default(),
            context_registry: None,
            style: None,
            entities: None,
            pause_signal: None,
            runtime_settings: None,
        }
    }

    fn segment(id: &str, ordinal: usize, blocks: Vec<(&str, &str)>) -> Segment {
        let segment_blocks: Vec<SegmentBlock> = blocks
            .iter()
            .map(|(block_id, text)| SegmentBlock {
                block_id: BlockId((*block_id).to_string()),
                kind: "paragraph".to_string(),
                text: (*text).to_string(),
                text_runs: vec![SegmentTextRun {
                    id: format!("r_{block_id}_0"),
                    text: (*text).to_string(),
                }],
                protected_spans: Vec::new(),
            })
            .collect();
        let block_ids = segment_blocks
            .iter()
            .map(|block| block.block_id.clone())
            .collect::<Vec<_>>();
        let source_text = segment_blocks
            .iter()
            .map(|block| block.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        Segment {
            id: SegmentId(id.to_string()),
            section_id: SectionId("sec_000000".to_string()),
            ordinal,
            block_ids,
            source: SegmentSource {
                text: source_text,
                blocks: segment_blocks,
                token_estimate: 4,
            },
            context: SegmentContext::default(),
            metadata: SegmentMetadata {
                book_title: Some("Test".to_string()),
                section_title: Some("Chapter".to_string()),
                section_index: 0,
                segment_index_in_section: 0,
                total_segments_in_section: 1,
            },
            constraints: SegmentConstraints {
                preserve_markers: Vec::new(),
                preserve_spans: Vec::new(),
                max_tokens: 100,
            },
            checksum: id.to_string(),
        }
    }

    fn protected_span(kind: ProtectedSpanKind, text: &str) -> ProtectedSpan {
        ProtectedSpan {
            kind,
            text: text.to_string(),
        }
    }

    #[derive(Debug, Clone)]
    struct FailingProvider;

    impl LlmProvider for FailingProvider {
        async fn complete(&self, _request: CompletionRequest) -> Result<CompletionResponse> {
            Err(LlmError::Provider("provider offline".to_string()))
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                supports_json_response_format: true,
                supports_usage_tokens: false,
            }
        }
    }

    #[derive(Debug, Clone)]
    struct WhitespaceTranslationProvider;

    impl LlmProvider for WhitespaceTranslationProvider {
        async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
            Ok(CompletionResponse {
                content: serde_json::json!({
                    "segment_id": request.metadata.segment_id.unwrap_or_default(),
                    "translation": " \n\t",
                })
                .to_string(),
                input_tokens: Some(1),
                input_cached_tokens: Some(0),
                output_tokens: Some(1),
                finish_reason: FinishReason::Stop,
                provider_latency_ms: 0,
                raw: serde_json::json!({}),
            })
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                supports_json_response_format: true,
                supports_usage_tokens: true,
            }
        }
    }

    #[derive(Debug, Clone)]
    struct BlockingProvider {
        started: Arc<AtomicUsize>,
        release: Arc<tokio::sync::Notify>,
    }

    #[derive(Debug, Clone)]
    struct GatedProvider {
        started: Arc<AtomicUsize>,
        release: Arc<tokio::sync::Semaphore>,
        metadata: Arc<std::sync::Mutex<Vec<RequestMetadata>>>,
    }

    impl GatedProvider {
        fn new() -> Self {
            Self {
                started: Arc::new(AtomicUsize::new(0)),
                release: Arc::new(tokio::sync::Semaphore::new(0)),
                metadata: Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }
    }

    impl LlmProvider for GatedProvider {
        async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
            self.started.fetch_add(1, Ordering::AcqRel);
            self.metadata
                .lock()
                .expect("metadata log mutex")
                .push(request.metadata.clone());
            self.release
                .acquire()
                .await
                .expect("test gate should remain open")
                .forget();
            Ok(CompletionResponse {
                content: serde_json::json!({
                    "segment_id": request.metadata.segment_id.unwrap_or_default(),
                    "translation": "Tradotto"
                })
                .to_string(),
                input_tokens: Some(1),
                input_cached_tokens: Some(0),
                output_tokens: Some(1),
                finish_reason: FinishReason::Stop,
                provider_latency_ms: 0,
                raw: serde_json::json!({}),
            })
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                supports_json_response_format: true,
                supports_usage_tokens: true,
            }
        }
    }

    impl BlockingProvider {
        fn new() -> Self {
            Self {
                started: Arc::new(AtomicUsize::new(0)),
                release: Arc::new(tokio::sync::Notify::new()),
            }
        }
    }

    impl LlmProvider for BlockingProvider {
        async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
            let count = self.started.fetch_add(1, Ordering::AcqRel);
            if count == 0 {
                self.release.notified().await;
            }
            Ok(CompletionResponse {
                content: serde_json::json!({
                    "segment_id": request.metadata.segment_id.unwrap_or_default(),
                    "translation": "Tradotto"
                })
                .to_string(),
                input_tokens: Some(1),
                input_cached_tokens: Some(0),
                output_tokens: Some(1),
                finish_reason: FinishReason::Stop,
                provider_latency_ms: 0,
                raw: serde_json::json!({}),
            })
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                supports_json_response_format: true,
                supports_usage_tokens: true,
            }
        }
    }

    async fn wait_for_request_count(started: &AtomicUsize, expected: usize) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while started.load(Ordering::Acquire) < expected {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("request count should be reached");
    }

    async fn wait_for_pause(signal: &PauseSignal) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while signal.state() != PauseState::Paused {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("signal should pause");
    }

    #[derive(Debug, Clone)]
    struct MissingProtectedSpanProvider;

    impl LlmProvider for MissingProtectedSpanProvider {
        async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
            Ok(CompletionResponse {
                content: serde_json::json!({
                    "segment_id": request.metadata.segment_id.unwrap_or_default(),
                    "translation": "Il giorno"
                })
                .to_string(),
                input_tokens: Some(10),
                input_cached_tokens: Some(0),
                output_tokens: Some(3),
                finish_reason: FinishReason::Stop,
                provider_latency_ms: 0,
                raw: serde_json::json!({}),
            })
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                supports_json_response_format: true,
                supports_usage_tokens: true,
            }
        }
    }

    fn segment_in_section(id: &str, section_id: &str, ordinal: usize, block_text: &str) -> Segment {
        let mut seg = segment(id, ordinal, vec![("b0", block_text)]);
        seg.section_id = SectionId(section_id.to_string());
        seg
    }

    fn translation_for(seg: &Segment, text: &str, status: SegmentStatus) -> SegmentTranslation {
        SegmentTranslation {
            segment_id: seg.id.clone(),
            ordinal: seg.ordinal,
            block_ids: seg.block_ids.clone(),
            blocks: seg
                .block_ids
                .iter()
                .map(|id| BlockTranslation {
                    block_id: id.clone(),
                    text: text.to_string(),
                })
                .collect(),
            checksum: seg.checksum.clone(),
            status,
            template: "translate_segment".to_string(),
            error: None,
            input_tokens: None,
            input_cached_tokens: None,
            output_tokens: None,
            tokens_estimated: false,
        }
    }

    #[tokio::test]
    async fn context_registry_returns_prior_pairs_in_closest_first_order() {
        let segs = vec![
            segment_in_section("a", "s1", 0, "Alpha"),
            segment_in_section("b", "s1", 1, "Bravo"),
            segment_in_section("c", "s1", 2, "Charlie"),
            segment_in_section("d", "s1", 3, "Delta"),
        ];
        let registry = ContextRegistry::new(&segs);
        for seg in &segs[..3] {
            registry.pre_populate(
                seg,
                &translation_for(
                    seg,
                    &format!("[T]{}", seg.source.text),
                    SegmentStatus::Succeeded,
                ),
            );
        }
        let cfg = ContextRunConfig {
            window: 3,
            budget_tokens: 10_000,
            scope: ContextScope::Chapter,
            strict: false,
        };
        let pairs = registry.await_context_for(&segs[3].id, cfg).await;
        let ids: Vec<&str> = pairs.iter().map(|p| p.segment_id.0.as_str()).collect();
        assert_eq!(ids, vec!["c", "b", "a"], "closest segment must come first");
    }

    #[tokio::test]
    async fn context_registry_skips_failed_status() {
        let segs = vec![
            segment_in_section("a", "s1", 0, "Alpha"),
            segment_in_section("b", "s1", 1, "Bravo"),
            segment_in_section("c", "s1", 2, "Charlie"),
        ];
        let registry = ContextRegistry::new(&segs);
        registry.pre_populate(
            &segs[0],
            &translation_for(&segs[0], "[T]Alpha", SegmentStatus::Succeeded),
        );
        registry.pre_populate(
            &segs[1],
            &translation_for(&segs[1], "[T]Bravo", SegmentStatus::Failed),
        );
        let cfg = ContextRunConfig {
            window: 3,
            budget_tokens: 10_000,
            scope: ContextScope::Chapter,
            strict: false,
        };
        let pairs = registry.await_context_for(&segs[2].id, cfg).await;
        let ids: Vec<&str> = pairs.iter().map(|p| p.segment_id.0.as_str()).collect();
        assert_eq!(
            ids,
            vec!["a"],
            "failed segment must unblock the fence but be filtered out"
        );
    }

    #[tokio::test]
    async fn context_registry_chapter_scope_excludes_other_sections() {
        let segs = vec![
            segment_in_section("a", "s1", 0, "Alpha"),
            segment_in_section("b", "s2", 1, "Bravo"),
            segment_in_section("c", "s2", 2, "Charlie"),
        ];
        let registry = ContextRegistry::new(&segs);
        for seg in &segs[..2] {
            registry.pre_populate(
                seg,
                &translation_for(
                    seg,
                    &format!("[T]{}", seg.source.text),
                    SegmentStatus::Succeeded,
                ),
            );
        }
        let cfg = ContextRunConfig {
            window: 3,
            budget_tokens: 10_000,
            scope: ContextScope::Chapter,
            strict: false,
        };
        let pairs = registry.await_context_for(&segs[2].id, cfg).await;
        let ids: Vec<&str> = pairs.iter().map(|p| p.segment_id.0.as_str()).collect();
        assert_eq!(
            ids,
            vec!["b"],
            "chapter scope must exclude cross-section segments"
        );
    }

    #[tokio::test]
    async fn context_registry_book_scope_walks_global_order() {
        let segs = vec![
            segment_in_section("a", "s1", 0, "Alpha"),
            segment_in_section("b", "s2", 1, "Bravo"),
            segment_in_section("c", "s2", 2, "Charlie"),
        ];
        let registry = ContextRegistry::new(&segs);
        for seg in &segs[..2] {
            registry.pre_populate(
                seg,
                &translation_for(
                    seg,
                    &format!("[T]{}", seg.source.text),
                    SegmentStatus::Succeeded,
                ),
            );
        }
        let cfg = ContextRunConfig {
            window: 3,
            budget_tokens: 10_000,
            scope: ContextScope::Book,
            strict: false,
        };
        let pairs = registry.await_context_for(&segs[2].id, cfg).await;
        let ids: Vec<&str> = pairs.iter().map(|p| p.segment_id.0.as_str()).collect();
        assert_eq!(
            ids,
            vec!["b", "a"],
            "book scope must walk segments across sections in canonical order"
        );
    }

    #[tokio::test]
    async fn context_budget_drops_oldest_pairs_first() {
        let segs = vec![
            segment_in_section("a", "s1", 0, "AlphaAlphaAlphaAlphaAlphaAlphaAlphaAlpha"),
            segment_in_section("b", "s1", 1, "BravoBravoBravoBravoBravoBravoBravoBravo"),
            segment_in_section("c", "s1", 2, "CharlieCharlieCharlieCharlieCharlieCharlie"),
            segment_in_section("d", "s1", 3, "Delta"),
        ];
        let registry = ContextRegistry::new(&segs);
        for seg in &segs[..3] {
            registry.pre_populate(
                seg,
                &translation_for(
                    seg,
                    &format!("[T]{} long-target-text-that-eats-budget", seg.source.text),
                    SegmentStatus::Succeeded,
                ),
            );
        }
        let cfg = ContextRunConfig {
            window: 3,
            budget_tokens: 30, // intentionally tight
            scope: ContextScope::Chapter,
            strict: false,
        };
        let pairs = registry.await_context_for(&segs[3].id, cfg).await;
        assert!(
            pairs.len() < 3,
            "budget should cap pairs (got {})",
            pairs.len()
        );
        if !pairs.is_empty() {
            // Closest-first: the kept entries must start at segment c (closest).
            assert_eq!(pairs[0].segment_id.0, "c");
        }
    }

    #[tokio::test]
    async fn context_registry_disabled_returns_empty() {
        let segs = vec![
            segment_in_section("a", "s1", 0, "Alpha"),
            segment_in_section("b", "s1", 1, "Bravo"),
        ];
        let registry = ContextRegistry::new(&segs);
        registry.pre_populate(
            &segs[0],
            &translation_for(&segs[0], "[T]Alpha", SegmentStatus::Succeeded),
        );
        let cfg = ContextRunConfig {
            window: 0,
            budget_tokens: 1200,
            scope: ContextScope::Chapter,
            strict: false,
        };
        let pairs = registry.await_context_for(&segs[1].id, cfg).await;
        assert!(pairs.is_empty());
    }

    #[tokio::test]
    async fn context_registry_waits_for_late_publish() {
        let segs = vec![
            segment_in_section("a", "s1", 0, "Alpha"),
            segment_in_section("b", "s1", 1, "Bravo"),
        ];
        let registry = Arc::new(ContextRegistry::new(&segs));
        let registry_clone = registry.clone();
        let seg_a = segs[0].clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            registry_clone.pre_populate(
                &seg_a,
                &translation_for(&seg_a, "[T]Alpha-late", SegmentStatus::Succeeded),
            );
        });
        let cfg = ContextRunConfig {
            window: 3,
            budget_tokens: 10_000,
            scope: ContextScope::Chapter,
            strict: true,
        };
        let pairs = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            registry.await_context_for(&segs[1].id, cfg),
        )
        .await
        .expect("fence must unblock once segment a publishes");
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].translated_text, "[T]Alpha-late");
    }

    #[tokio::test]
    async fn best_effort_context_returns_partial_pairs_without_waiting() {
        let segs = vec![
            segment_in_section("a", "s1", 0, "Alpha"),
            segment_in_section("b", "s1", 1, "Bravo"),
            segment_in_section("c", "s1", 2, "Charlie"),
            segment_in_section("d", "s1", 3, "Delta"),
        ];
        let registry = ContextRegistry::new(&segs);
        // Only the closest predecessor has completed; a and b are still
        // in flight.
        registry.pre_populate(
            &segs[2],
            &translation_for(&segs[2], "[T]Charlie", SegmentStatus::Succeeded),
        );
        let cfg = ContextRunConfig {
            window: 3,
            budget_tokens: 10_000,
            scope: ContextScope::Chapter,
            strict: false,
        };
        let pairs = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            registry.await_context_for(&segs[3].id, cfg),
        )
        .await
        .expect("best-effort context must not block on missing predecessors");
        let ids: Vec<&str> = pairs.iter().map(|p| p.segment_id.0.as_str()).collect();
        assert_eq!(ids, vec!["c"], "only completed predecessors are returned");
    }

    #[test]
    fn render_context_pairs_formats_chronological() {
        let segs = [
            segment_in_section("a", "s1", 0, "Alpha"),
            segment_in_section("b", "s1", 1, "Bravo"),
        ];
        let pairs = [
            CompletedContext {
                segment_id: segs[1].id.clone(),
                section_id: segs[1].section_id.clone(),
                ordinal: 1,
                source_text: "Bravo source".to_string(),
                translated_text: "Bravo target".to_string(),
                status: SegmentStatus::Succeeded,
                source_token_estimate: 2,
            },
            CompletedContext {
                segment_id: segs[0].id.clone(),
                section_id: segs[0].section_id.clone(),
                ordinal: 0,
                source_text: "Alpha source".to_string(),
                translated_text: "Alpha target".to_string(),
                status: SegmentStatus::Succeeded,
                source_token_estimate: 2,
            },
        ];
        let rendered = render_context_pairs(&pairs);
        // Closest-first input becomes chronological in output (oldest at top).
        let alpha_pos = rendered.find("Alpha source").expect("alpha present");
        let bravo_pos = rendered.find("Bravo source").expect("bravo present");
        assert!(alpha_pos < bravo_pos, "older segment must render first");
        assert!(rendered.contains("=== Context"));
        assert!(rendered.contains("=== End context ==="));
    }

    #[test]
    fn render_context_pairs_empty_returns_empty() {
        assert_eq!(render_context_pairs(&[]), "");
    }

    #[derive(Clone, Default)]
    struct PromptCaptureProvider {
        // segment_id -> last user prompt seen
        log: Arc<std::sync::Mutex<HashMap<String, String>>>,
    }

    impl LlmProvider for PromptCaptureProvider {
        async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
            let segment_id = request
                .metadata
                .segment_id
                .clone()
                .unwrap_or_else(|| "unknown".to_string());
            {
                let mut log = self.log.lock().expect("prompt log mutex poisoned");
                log.insert(segment_id.clone(), request.user.clone());
            }
            // Emit a minimal valid plain-mode JSON response so the validator passes.
            let body = serde_json::json!({
                "segment_id": segment_id,
                "translation": format!("[T]{segment_id}"),
            });
            Ok(CompletionResponse {
                content: body.to_string(),
                input_tokens: Some(10),
                input_cached_tokens: Some(0),
                output_tokens: Some(5),
                finish_reason: FinishReason::Stop,
                provider_latency_ms: 0,
                raw: serde_json::json!({}),
            })
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                supports_json_response_format: true,
                supports_usage_tokens: true,
            }
        }
    }

    #[tokio::test]
    async fn sliding_context_inserts_prior_pairs_into_segment_prompt() {
        let segs = vec![
            segment_in_section("s0", "ch1", 0, "First sentence."),
            segment_in_section("s1", "ch1", 1, "Second sentence."),
            segment_in_section("s2", "ch1", 2, "Third sentence."),
            segment_in_section("s3", "ch1", 3, "Fourth sentence."),
        ];
        let registry = Arc::new(ContextRegistry::new(&segs));
        let provider = PromptCaptureProvider::default();
        let log = provider.log.clone();

        let mut cfg = config();
        cfg.scheduler.concurrency = 1; // deterministic ordering
        cfg.context = ContextRunConfig {
            window: 3,
            budget_tokens: 10_000,
            scope: ContextScope::Chapter,
            strict: true,
        };
        cfg.context_registry = Some(registry.clone());

        let translations = translate_segments(provider, &segs, &cfg)
            .await
            .expect("mock translation should succeed");
        assert_eq!(translations.len(), 4);
        for t in &translations {
            assert_eq!(t.status, SegmentStatus::Succeeded);
        }

        let log = log.lock().expect("log");
        let s3_prompt = log.get("s3").expect("s3 prompt captured");
        assert!(
            s3_prompt.contains("=== Context (already translated"),
            "s3 prompt must include sliding-context block"
        );
        assert!(
            s3_prompt.contains("First sentence."),
            "context for s3 must include earliest in-window source"
        );
        assert!(
            s3_prompt.contains("Second sentence."),
            "context for s3 must include second source"
        );
        assert!(
            s3_prompt.contains("Third sentence."),
            "context for s3 must include third source (closest)"
        );

        let s0_prompt = log.get("s0").expect("s0 prompt captured");
        assert!(
            !s0_prompt.contains("=== Context"),
            "first segment has no prior context"
        );
    }
}
