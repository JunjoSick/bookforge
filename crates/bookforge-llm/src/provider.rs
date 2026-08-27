use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use tokio::time::{Duration, sleep};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use bookforge_core::{RetryAfterPolicy, marker::is_marker_token};

pub type Result<T> = std::result::Result<T, LlmError>;

/// Translation responses are request-bounded in normal use; 4 MiB leaves
/// ample room for provider metadata while preventing hostile endpoints from
/// growing the process without limit.
const MAX_PROVIDER_RESPONSE_BODY_BYTES: usize = 4 * 1024 * 1024;
/// Provider error bodies can be verbose, but only a short diagnostic belongs
/// in errors and logs.
const MAX_PROVIDER_ERROR_BODY_BYTES: usize = 8 * 1024;

/// Upper bound on a `Retry-After` hint we are willing to honor, whether the
/// server sent delay-seconds or an HTTP-date. A hostile or buggy endpoint can
/// otherwise stall one request (and with it a whole worker) for hours. Five
/// minutes is generous enough for every documented rate-limit window we have
/// seen in practice while staying far below the old 60s cap's loss of intent:
/// hints beyond the cap are clamped rather than discarded, so the spirit of
/// the header survives.
const MAX_HONORED_RETRY_AFTER_SECS: u64 = 300;

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("provider error: {0}")]
    Provider(String),

    #[error("invalid response: {0}")]
    InvalidResponse(String),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("HTTP status {status}: {body}")]
    HttpStatus { status: u16, body: String },
}

pub trait LlmProvider: Send + Sync + 'static {
    fn complete(
        &self,
        request: CompletionRequest,
    ) -> impl std::future::Future<Output = Result<CompletionResponse>> + Send;

    fn capabilities(&self) -> ProviderCapabilities;

    /// Whether this provider/model is a reasoning (chain-of-thought) model
    /// that consumes part of the `max_tokens` budget for internal reasoning.
    /// Defaults to `false`.
    fn is_reasoning(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderCapabilities {
    pub supports_json_response_format: bool,
    pub supports_usage_tokens: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionRequest {
    pub system: String,
    pub user: String,
    pub response_format: ResponseFormat,
    pub temperature: f32,
    pub max_output_tokens: Option<u32>,
    pub metadata: RequestMetadata,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResponseFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestMetadata {
    pub segment_id: Option<String>,
    pub block_ids: Vec<String>,
    pub prompt_template: Option<String>,
    pub prompt_version: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub source_checksum: Option<String>,
    /// Runtime override revision frozen at the request boundary. `None` is
    /// retained for library callers that do not opt into live settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_config_revision: Option<u64>,
    /// Provider-internal attempt limit frozen for this complete call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_max_attempts: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionResponse {
    pub content: String,
    pub input_tokens: Option<u64>,
    pub input_cached_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub finish_reason: FinishReason,
    pub provider_latency_ms: u64,
    pub raw: serde_json::Value,
}

impl CompletionResponse {
    /// Returns `true` when the API returned `reasoning_content` in any choice,
    /// indicating the model is a reasoning / chain-of-thought model that consumes
    /// part of the `max_tokens` budget for internal reasoning.
    pub fn is_reasoning_response(&self) -> bool {
        self.raw
            .pointer("/choices/0/message/reasoning_content")
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FinishReason {
    Stop,
    Length,
    ContentFilter,
    ToolCalls,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockMode {
    Identity,
    PrefixTarget,
    Uppercase,
    MalformedJson,
    WrongSegmentId,
}

#[derive(Debug, Clone)]
pub struct MockProvider {
    mode: MockMode,
    target_language: String,
}

impl MockProvider {
    pub fn new(mode: MockMode, target_language: impl Into<String>) -> Self {
        Self {
            mode,
            target_language: target_language.into(),
        }
    }
}

impl LlmProvider for MockProvider {
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        let started = Instant::now();
        let template = request
            .metadata
            .prompt_template
            .as_deref()
            .unwrap_or("translate_segment");
        if let Some(delay_ms) = mock_delay_ms(template) {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        }
        await_mock_release_gate().await;

        if self.mode == MockMode::MalformedJson {
            return Ok(CompletionResponse {
                content: "{not valid json".to_string(),
                input_tokens: Some(estimate_tokens(&request.user)),
                input_cached_tokens: Some(0),
                output_tokens: None,
                finish_reason: FinishReason::Stop,
                provider_latency_ms: started.elapsed().as_millis() as u64,
                raw: json!({"provider": "mock", "mode": "malformed_json"}),
            });
        }

        let block_ids = &request.metadata.block_ids;

        let content = if template == "glossary_propose" {
            let proposals = extract_json_array_after_label(&request.user, "Candidates:")
                .into_iter()
                .filter_map(|entry| {
                    let id = entry.get("id")?.as_i64()?;
                    let source = entry
                        .get("source_text")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default();
                    Some(json!({
                        "id": id,
                        "target_text": transform_text(
                            self.mode,
                            &self.target_language,
                            source,
                        ),
                        "policy": "recreate",
                        "reason": "Mock proposal for offline testing.",
                    }))
                })
                .collect::<Vec<_>>();
            serde_json::to_string(&json!({ "proposals": proposals }))?
        } else if template == "qa_batch" {
            let reviews = extract_qa_batch_item_ids(&request.user)
                .into_iter()
                .map(|id| {
                    let review_id = if self.mode == MockMode::WrongSegmentId {
                        "wrong_segment".to_string()
                    } else {
                        id
                    };
                    json!({
                        "id": review_id,
                        "verdict": "pass",
                        "issues": [],
                    })
                })
                .collect::<Vec<_>>();
            serde_json::to_string(&json!({ "reviews": reviews }))?
        } else if template == "double_check_batch" {
            let force_fail = std::env::var("BOOKFORGE_MOCK_DOUBLE_CHECK_FAIL").is_ok();
            let items = extract_batch_items(&request.user)
                .into_iter()
                .filter_map(|entry| {
                    let id = entry.get("id")?.as_str()?.to_string();
                    let issues = if force_fail {
                        vec![json!({
                            "severity": "major",
                            "kind": "mock_double_check",
                            "message": "mock double-check requested correction",
                            "source_excerpt": null,
                            "translation_excerpt": null,
                            "needs_correction": true,
                        })]
                    } else {
                        Vec::new()
                    };
                    Some(json!({
                        "id": id,
                        "verdict": if force_fail { "fail" } else { "pass" },
                        "issues": issues,
                    }))
                })
                .collect::<Vec<_>>();
            serde_json::to_string(&json!({ "items": items }))?
        } else if template == "correct_batch" {
            let items = extract_json_array_after_label(&request.user, "Items:")
                .into_iter()
                .filter_map(|entry| {
                    let id = entry.get("id")?.as_str()?.to_string();
                    let current = entry
                        .get("current_translation")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default();
                    Some(json!({
                        "id": id,
                        "corrected_translation": format!("{current} [corrected]"),
                    }))
                })
                .collect::<Vec<_>>();
            serde_json::to_string(&json!({ "items": items }))?
        } else if template.starts_with("translate_batch_") {
            let items = extract_batch_items(&request.user)
                .into_iter()
                .filter_map(|entry| {
                    let id = entry.get("id")?.as_str()?.to_string();
                    let response_id = if self.mode == MockMode::WrongSegmentId {
                        "wrong_segment".to_string()
                    } else {
                        id
                    };
                    if template.contains("run_preserving") {
                        let runs = entry
                            .get("runs")
                            .and_then(serde_json::Value::as_array)
                            .map(|runs| {
                                runs.iter()
                                    .filter_map(|run| {
                                        let id = run.get("id")?.as_str()?;
                                        let source = run.get("text")?.as_str().unwrap_or_default();
                                        Some(json!({
                                            "id": id,
                                            "text": transform_run_text(
                                                self.mode,
                                                &self.target_language,
                                                source,
                                            ),
                                        }))
                                    })
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default();
                        Some(json!({
                            "id": response_id,
                            "runs": runs,
                        }))
                    } else {
                        let source = entry
                            .get("text")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default();
                        Some(json!({
                            "id": response_id,
                            "translation": transform_text(
                                self.mode,
                                &self.target_language,
                                source,
                            ),
                        }))
                    }
                })
                .collect::<Vec<_>>();
            serde_json::to_string(&json!({ "items": items }))?
        } else {
            let segment_id = request.metadata.segment_id.clone().ok_or_else(|| {
                LlmError::Provider("mock request is missing segment_id".to_string())
            })?;
            let response_segment_id = if self.mode == MockMode::WrongSegmentId {
                "wrong_segment".to_string()
            } else {
                segment_id
            };

            match template {
                "translate_marker_safe" => {
                    let block_sources = extract_block_sources_from_json(&request.user, block_ids);
                    let blocks = block_ids
                        .iter()
                        .map(|block_id| {
                            let source = block_sources
                                .get(block_id.as_str())
                                .cloned()
                                .unwrap_or_default();
                            let translated =
                                transform_text(self.mode, &self.target_language, &source);
                            json!({
                                "block_id": block_id,
                                "translation": translated,
                            })
                        })
                        .collect::<Vec<_>>();
                    serde_json::to_string(&json!({
                        "segment_id": response_segment_id,
                        "blocks": blocks,
                    }))?
                }
                "translate_run_preserving" => {
                    let run_sources = extract_run_sources_from_json(&request.user, block_ids);
                    let blocks = block_ids
                        .iter()
                        .map(|block_id| {
                            let runs = run_sources
                                .get(block_id.as_str())
                                .cloned()
                                .unwrap_or_default()
                                .into_iter()
                                .map(|(id, source)| {
                                    json!({
                                        "id": id,
                                        "text": transform_run_text(
                                            self.mode,
                                            &self.target_language,
                                            &source,
                                        ),
                                    })
                                })
                                .collect::<Vec<_>>();
                            json!({
                                "block_id": block_id,
                                "translated_runs": runs,
                            })
                        })
                        .collect::<Vec<_>>();
                    serde_json::to_string(&json!({
                        "segment_id": response_segment_id,
                        "blocks": blocks,
                    }))?
                }
                "qa_segment" => serde_json::to_string(&json!({
                    "segment_id": response_segment_id,
                    "verdict": "pass",
                    "issues": [],
                }))?,
                _ => {
                    let source =
                        extract_plain_source(&request.user).unwrap_or_else(|| request.user.clone());
                    let translation = transform_text(self.mode, &self.target_language, &source);
                    serde_json::to_string(&json!({
                        "segment_id": response_segment_id,
                        "translation": translation,
                    }))?
                }
            }
        };

        Ok(CompletionResponse {
            input_tokens: Some(estimate_tokens(&request.user)),
            input_cached_tokens: Some(0),
            output_tokens: Some(estimate_tokens(&content)),
            finish_reason: FinishReason::Stop,
            provider_latency_ms: started.elapsed().as_millis() as u64,
            raw: json!({
                "provider": "mock",
                "mode": format!("{:?}", self.mode),
                "template": template,
            }),
            content,
        })
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_json_response_format: true,
            supports_usage_tokens: true,
        }
    }
}

fn transform_run_text(mode: MockMode, target_language: &str, source: &str) -> String {
    if is_marker_token(source) {
        source.to_string()
    } else {
        transform_text(mode, target_language, source)
    }
}

fn transform_text(mode: MockMode, target_language: &str, source: &str) -> String {
    match mode {
        MockMode::Identity | MockMode::WrongSegmentId => source.to_string(),
        MockMode::PrefixTarget => format!("[{target_language}] {source}"),
        MockMode::Uppercase => source.to_uppercase(),
        MockMode::MalformedJson => unreachable!("handled above"),
    }
}

/// Park every mock request until the file named by `BOOKFORGE_MOCK_RELEASE_FILE`
/// exists.
///
/// `BOOKFORGE_MOCK_DELAY_MS` only widens a timing window; a lifecycle test that
/// needs a provider request to still be in flight while it drives a control-file
/// transition has to guess how much of that window the machine will eat, which
/// makes it flaky under parallel load. This gate turns the window into a
/// handshake: the run emits `RequestStarted`, blocks here, and the test releases
/// it once it has observed the state it wanted to set up. The elapsed bound only
/// exists so a test that panics before writing the release file cannot leave an
/// immortal child process behind.
async fn await_mock_release_gate() {
    let Ok(path) = std::env::var("BOOKFORGE_MOCK_RELEASE_FILE") else {
        return;
    };
    let path = std::path::PathBuf::from(path);
    let deadline = Instant::now() + Duration::from_secs(120);
    while !path.exists() {
        if Instant::now() >= deadline {
            warn!(
                path = %path.display(),
                "mock release gate timed out; continuing without release"
            );
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }
}

fn mock_delay_ms(template: &str) -> Option<u64> {
    let stage_env = match template {
        "qa_batch" | "qa_segment" => Some("BOOKFORGE_MOCK_QA_DELAY_MS"),
        "double_check_batch" => Some("BOOKFORGE_MOCK_DOUBLE_CHECK_DELAY_MS"),
        "correct_batch" => Some("BOOKFORGE_MOCK_CORRECTION_DELAY_MS"),
        _ => None,
    };
    stage_env
        .and_then(|name| std::env::var(name).ok())
        .or_else(|| std::env::var("BOOKFORGE_MOCK_DELAY_MS").ok())
        .and_then(|value| value.parse::<u64>().ok())
}

/// Recover per-block source strings from a rendered marker-safe user prompt.
/// The caller embeds blocks as a JSON array under `Source blocks:`; the mock
/// parses it back so test translations transform the actual source text rather
/// than the whole rendered prompt.
fn extract_block_sources_from_json<'a>(
    user_prompt: &str,
    block_ids: &'a [String],
) -> std::collections::BTreeMap<&'a str, String> {
    let mut sources = std::collections::BTreeMap::new();
    let Some(marker) = user_prompt.find("Source blocks:") else {
        return sources;
    };
    let after = &user_prompt[marker..];
    let Some(start) = after.find('[') else {
        return sources;
    };
    let array_slice = &after[start..];
    let mut depth = 0usize;
    let mut end_index = None;
    for (offset, ch) in array_slice.char_indices() {
        match ch {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    end_index = Some(offset + ch.len_utf8());
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(end) = end_index else {
        return sources;
    };
    let array_text = &array_slice[..end];
    let parsed: Vec<serde_json::Value> = match serde_json::from_str(array_text) {
        Ok(value) => value,
        Err(_) => return sources,
    };
    for entry in parsed {
        let Some(block_id) = entry.get("block_id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let text = entry
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        if let Some(known) = block_ids.iter().find(|known| known.as_str() == block_id) {
            sources.insert(known.as_str(), text);
        }
    }
    sources
}

fn extract_run_sources_from_json<'a>(
    user_prompt: &str,
    block_ids: &'a [String],
) -> std::collections::BTreeMap<&'a str, Vec<(String, String)>> {
    let mut sources = std::collections::BTreeMap::new();
    let Some(marker) = user_prompt.find("Source blocks and runs:") else {
        return sources;
    };
    let after = &user_prompt[marker..];
    let Some(start) = after.find('[') else {
        return sources;
    };
    let array_slice = &after[start..];
    let mut depth = 0usize;
    let mut end_index = None;
    for (offset, ch) in array_slice.char_indices() {
        match ch {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    end_index = Some(offset + ch.len_utf8());
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(end) = end_index else {
        return sources;
    };
    let array_text = &array_slice[..end];
    let parsed: Vec<serde_json::Value> = match serde_json::from_str(array_text) {
        Ok(value) => value,
        Err(_) => return sources,
    };

    for entry in parsed {
        let Some(block_id) = entry.get("block_id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(known) = block_ids.iter().find(|known| known.as_str() == block_id) else {
            continue;
        };
        let runs = entry
            .get("runs")
            .and_then(serde_json::Value::as_array)
            .map(|runs| {
                runs.iter()
                    .filter_map(|run| {
                        Some((
                            run.get("id")?.as_str()?.to_string(),
                            run.get("text")?.as_str()?.to_string(),
                        ))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        sources.insert(known.as_str(), runs);
    }

    sources
}

fn extract_qa_batch_item_ids(user_prompt: &str) -> Vec<String> {
    user_prompt
        .lines()
        .rev()
        .find_map(|line| {
            let trimmed = line.trim_start();
            if !trimmed.starts_with('[') {
                return None;
            }
            let parsed: Vec<serde_json::Value> = serde_json::from_str(trimmed).ok()?;
            let ids = parsed
                .into_iter()
                .filter_map(|entry| {
                    entry
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .map(ToString::to_string)
                })
                .collect::<Vec<_>>();
            if ids.is_empty() { None } else { Some(ids) }
        })
        .unwrap_or_default()
}

fn extract_batch_items(user_prompt: &str) -> Vec<serde_json::Value> {
    user_prompt
        .lines()
        .rev()
        .find_map(|line| {
            let trimmed = line.trim_start();
            if !trimmed.starts_with('[') {
                return None;
            }
            let parsed: Vec<serde_json::Value> = serde_json::from_str(trimmed).ok()?;
            if parsed.is_empty() {
                None
            } else {
                Some(parsed)
            }
        })
        .unwrap_or_default()
}

fn extract_json_array_after_label(user_prompt: &str, label: &str) -> Vec<serde_json::Value> {
    let Some(label_index) = user_prompt.find(label) else {
        return Vec::new();
    };
    let after_label = &user_prompt[label_index + label.len()..];
    let Some(start) = after_label.find('[') else {
        return Vec::new();
    };
    let array_slice = &after_label[start..];
    let mut depth = 0usize;
    let mut end_index = None;
    for (offset, ch) in array_slice.char_indices() {
        match ch {
            '[' => depth += 1,
            ']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    end_index = Some(offset + ch.len_utf8());
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(end) = end_index else {
        return Vec::new();
    };
    serde_json::from_str(&array_slice[..end]).unwrap_or_default()
}

/// Recover the plain-mode source segment from a rendered prompt by reading
/// the contents of the ` ```txt ... ``` ` block that follows `Source segment:`.
fn extract_plain_source(user_prompt: &str) -> Option<String> {
    let header = user_prompt.find("Source segment:")?;
    let after_header = &user_prompt[header..];
    let fence = after_header.find("```txt")?;
    let after_fence = &after_header[fence + "```txt".len()..];
    let body_start = after_fence.find('\n').map(|n| n + 1)?;
    let body = &after_fence[body_start..];
    let end = body.find("```")?;
    Some(body[..end].trim_end_matches('\n').to_string())
}

fn estimate_tokens(text: &str) -> u64 {
    bookforge_core::segment::estimate_tokens(text).max(1) as u64
}

fn cached_input_tokens(raw: &Value) -> Option<u64> {
    raw.pointer("/usage/prompt_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .or_else(|| {
            raw.pointer("/usage/input_tokens_details/cached_tokens")
                .and_then(Value::as_u64)
        })
        .or_else(|| {
            raw.pointer("/usage/input_token_details/cache_read")
                .and_then(Value::as_u64)
        })
        .or_else(|| {
            raw.pointer("/usage/cache_read_input_tokens")
                .and_then(Value::as_u64)
        })
        .or(Some(0))
}

fn reasoning_tokens(raw: &Value) -> Option<u64> {
    raw.pointer("/usage/completion_tokens_details/reasoning_tokens")
        .and_then(Value::as_u64)
        .or_else(|| {
            raw.pointer("/usage/output_tokens_details/reasoning_tokens")
                .and_then(Value::as_u64)
        })
}

/// Return the provider's billable output-token aggregate.
///
/// OpenAI, OpenRouter, and DeepSeek define reasoning tokens as a subset of
/// `completion_tokens`, so adding the detailed count would double-count them.
/// A few OpenAI-compatible gateways have returned visible completion tokens
/// separately while keeping `total_tokens` correct, however. Taking the larger
/// of `completion_tokens` and `total_tokens - prompt_tokens` handles both shapes
/// without double-counting standards-compliant responses. The reasoning detail
/// is a final fallback for partial usage objects.
fn billable_output_tokens(raw: &Value) -> Option<u64> {
    let completion_tokens = raw
        .pointer("/usage/completion_tokens")
        .and_then(Value::as_u64);
    let output_from_total = raw
        .pointer("/usage/total_tokens")
        .and_then(Value::as_u64)
        .zip(raw.pointer("/usage/prompt_tokens").and_then(Value::as_u64))
        .map(|(total, prompt)| total.saturating_sub(prompt));

    completion_tokens
        .into_iter()
        .chain(output_from_total)
        .max()
        .or_else(|| reasoning_tokens(raw))
}

#[derive(Debug, Clone)]
pub struct OpenAiCompatibleConfig {
    pub base_url: String,
    pub api_key_env: String,
    pub model: String,
    pub timeout_seconds: u64,
    pub provider_max_attempts: usize,
    pub thinking_disabled: bool,
    pub retry_after_policy: RetryAfterPolicy,
    pub max_backoff_seconds: u64,
    pub max_idle_per_host: usize,
    pub json_mode: bookforge_core::JsonMode,
}

impl OpenAiCompatibleConfig {
    pub fn deepseek(model: Option<String>) -> Self {
        Self {
            base_url: "https://api.deepseek.com/v1".to_string(),
            api_key_env: "DEEPSEEK_API_KEY".to_string(),
            model: model.unwrap_or_else(|| "deepseek-v4-flash".to_string()),
            timeout_seconds: 120,
            provider_max_attempts: 6,
            thinking_disabled: false,
            retry_after_policy: RetryAfterPolicy::JitteredExponential,
            max_backoff_seconds: 60,
            max_idle_per_host: 32,
            json_mode: bookforge_core::JsonMode::Auto,
        }
    }
}

#[derive(Debug)]
pub struct OpenAiCompatibleProvider {
    config: OpenAiCompatibleConfig,
    client: reqwest::Client,
    reasoning_detected: AtomicBool,
    response_format_supported: AtomicBool,
    thinking_warning_emitted: AtomicBool,
    collapsed_budget_warning_emitted: AtomicBool,
    pub cancel_token: CancellationToken,
}

impl Clone for OpenAiCompatibleProvider {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            client: self.client.clone(),
            reasoning_detected: AtomicBool::new(self.reasoning_detected.load(Ordering::Relaxed)),
            response_format_supported: AtomicBool::new(
                self.response_format_supported.load(Ordering::Relaxed),
            ),
            thinking_warning_emitted: AtomicBool::new(
                self.thinking_warning_emitted.load(Ordering::Relaxed),
            ),
            collapsed_budget_warning_emitted: AtomicBool::new(
                self.collapsed_budget_warning_emitted
                    .load(Ordering::Relaxed),
            ),
            cancel_token: self.cancel_token.clone(),
        }
    }
}

impl OpenAiCompatibleProvider {
    pub fn new(config: OpenAiCompatibleConfig) -> Result<Self> {
        Self::new_with_cancel(config, CancellationToken::new())
    }

    pub fn new_with_cancel(
        config: OpenAiCompatibleConfig,
        cancel_token: CancellationToken,
    ) -> Result<Self> {
        // Names alone are not the whole story (see `model_ships_with_thinking`):
        // a request that explicitly disables thinking never spends its output
        // budget on hidden chain-of-thought, so it needs neither the x3
        // multiplier nor the ≥300s timeout floor. Runtime detection on
        // `reasoning_content` still flips this back on if an endpoint thinks
        // anyway.
        let bootstrapped_reasoning = bootstrapped_reasoning(&config);
        let effective_timeout = if bootstrapped_reasoning {
            config.timeout_seconds.max(300)
        } else {
            config.timeout_seconds
        };
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(effective_timeout))
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(config.max_idle_per_host)
            .tcp_keepalive(Duration::from_secs(60))
            .build()?;
        Ok(Self {
            config,
            client,
            reasoning_detected: AtomicBool::new(bootstrapped_reasoning),
            response_format_supported: AtomicBool::new(true),
            thinking_warning_emitted: AtomicBool::new(false),
            collapsed_budget_warning_emitted: AtomicBool::new(false),
            cancel_token,
        })
    }

    pub fn model(&self) -> &str {
        &self.config.model
    }

    fn request_body(&self, request: &CompletionRequest) -> Value {
        let mut body = json!({
            "model": self.config.model,
            "temperature": request.temperature,
            "messages": [
                {"role": "system", "content": request.system},
                {"role": "user", "content": request.user}
            ]
        });

        if let Some(max_tokens) = request.max_output_tokens {
            // Audit LLM-P3c: a degenerate plan (context remainder clamped to
            // zero, e.g. a pathological segment bigger than the whole window)
            // must never serialize a zero-token request — that is a guaranteed
            // opaque HTTP 400 on most endpoints. Floor the wire value at 1 and
            // surface a plan warning so oversized segments are visible instead
            // of vanishing into a silent 400 chase.
            let limit = max_tokens.max(1);
            if max_tokens == 0
                && !self
                    .collapsed_budget_warning_emitted
                    .swap(true, Ordering::Relaxed)
            {
                warn!(
                    base_url = %self.config.base_url,
                    model = %self.config.model,
                    segment_id = ?request.metadata.segment_id,
                    "plan warning: output budget collapsed to 1 token — segment exceeds its \
                     context window; request sent with max_tokens=1 and will likely truncate"
                );
            }
            body["max_tokens"] = json!(limit);
        }

        if self.config.thinking_disabled {
            match reasoning_control_for_config(&self.config) {
                ReasoningControl::OpenRouter => {
                    body["reasoning"] = json!({"enabled": false});
                }
                ReasoningControl::OpenAi => {
                    body["reasoning_effort"] = json!("none");
                }
                ReasoningControl::DeepSeek => {
                    body["thinking"] = json!({"type": "disabled"});
                }
                ReasoningControl::Unsupported => {
                    if !self.thinking_warning_emitted.swap(true, Ordering::Relaxed) {
                        warn!(
                            base_url = %self.config.base_url,
                            model = %self.config.model,
                            "provider: thinking suppression was requested, but this \
                             OpenAI-compatible endpoint has no recognized suppression \
                             parameter; sending no thinking/reasoning field"
                        );
                    }
                }
            }
        }

        let use_response_format = request.response_format == ResponseFormat::Json
            && match self.config.json_mode {
                bookforge_core::JsonMode::PromptOnly => false,
                bookforge_core::JsonMode::ResponseFormat => true,
                bookforge_core::JsonMode::Auto => {
                    self.response_format_supported.load(Ordering::Relaxed)
                }
            };

        if use_response_format {
            body["response_format"] = json!({"type": "json_object"});
        }

        body
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReasoningControl {
    OpenRouter,
    OpenAi,
    DeepSeek,
    Unsupported,
}

fn reasoning_control_for_config(config: &OpenAiCompatibleConfig) -> ReasoningControl {
    let host = reqwest::Url::parse(&config.base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase));

    match host.as_deref() {
        Some(host) if host == "openrouter.ai" || host.ends_with(".openrouter.ai") => {
            ReasoningControl::OpenRouter
        }
        Some("api.openai.com") => ReasoningControl::OpenAi,
        Some("api.deepseek.com") => ReasoningControl::DeepSeek,
        _ => match config.api_key_env.as_str() {
            "OPENROUTER_API_KEY" => ReasoningControl::OpenRouter,
            "DEEPSEEK_API_KEY" => ReasoningControl::DeepSeek,
            _ => ReasoningControl::Unsupported,
        },
    }
}

impl LlmProvider for OpenAiCompatibleProvider {
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        let mut body = self.request_body(&request);
        let api_key = match std::env::var(&self.config.api_key_env) {
            Ok(value) => Some(value),
            Err(_) if local_api_key_is_optional(&self.config.api_key_env) => None,
            Err(_) => {
                return Err(LlmError::Provider(format!(
                    "environment variable '{}' is not set",
                    self.config.api_key_env
                )));
            }
        };
        let started = Instant::now();
        let endpoint = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );

        let use_response_format = request.response_format == ResponseFormat::Json
            && match self.config.json_mode {
                bookforge_core::JsonMode::PromptOnly => false,
                bookforge_core::JsonMode::ResponseFormat => true,
                bookforge_core::JsonMode::Auto => {
                    self.response_format_supported.load(Ordering::Relaxed)
                }
            };

        let max_attempts = request
            .metadata
            .provider_max_attempts
            .unwrap_or(self.config.provider_max_attempts)
            .max(1);
        let body_len = serde_json::to_string(&body).map(|s| s.len()).unwrap_or(0);
        let max_backoff = Duration::from_secs(self.config.max_backoff_seconds);
        let policy = self.config.retry_after_policy;
        let mut raw = None;
        let mut last_error = None;
        let mut tried_response_format_fallback = false;
        let mut attempt = 0usize;
        while attempt < max_attempts {
            let mut request_builder = self.client.post(&endpoint).json(&body);
            if let Some(api_key) = api_key.as_deref() {
                request_builder = request_builder.bearer_auth(api_key);
            }
            let send_future = request_builder.send();

            let response = match cancelable(&self.cancel_token, send_future).await {
                Ok(Ok(resp)) => resp,
                Ok(Err(error)) => {
                    let kind = if error.is_timeout() {
                        "timeout"
                    } else if error.is_connect() {
                        "connect"
                    } else if error.is_decode() {
                        "decode"
                    } else if error.is_body() {
                        "body"
                    } else {
                        "other"
                    };
                    warn!(
                        "provider: attempt {}/{} [{kind}] body={body_len}bytes: {error}",
                        attempt + 1,
                        max_attempts,
                    );
                    let retryable = is_retryable_http_error(&error);
                    let attempt_limit = attempt_limit_for_http_error(&error, max_attempts);
                    last_error = Some(LlmError::Http(error));
                    if !retryable {
                        return Err(last_error.expect("set above"));
                    }
                    attempt += 1;
                    if attempt >= attempt_limit {
                        return Err(last_error.expect("set above"));
                    }
                    apply_retry_delay(
                        &self.cancel_token,
                        policy,
                        attempt - 1,
                        None,
                        max_backoff,
                        last_error.take().expect("set above"),
                    )
                    .await?;
                    continue;
                }
                Err(_) => {
                    return Err(LlmError::Provider("interrupted by user".to_string()));
                }
            };
            let status = response.status();

            if status.is_success() {
                let response_bytes = match cancelable(
                    &self.cancel_token,
                    read_provider_response_body(response),
                )
                .await
                {
                    Ok(Ok(b)) => b,
                    Ok(Err(LlmError::Http(error))) => {
                        warn!(
                            "provider: attempt {}/{} body read failed (status={status:#}): {error}",
                            attempt + 1,
                            max_attempts,
                        );
                        let retryable = is_retryable_http_error(&error);
                        let attempt_limit = attempt_limit_for_http_error(&error, max_attempts);
                        last_error = Some(LlmError::Http(error));
                        if !retryable {
                            return Err(last_error.expect("set above"));
                        }
                        attempt += 1;
                        if attempt >= attempt_limit {
                            return Err(last_error.expect("set above"));
                        }
                        apply_retry_delay(
                            &self.cancel_token,
                            policy,
                            attempt - 1,
                            None,
                            max_backoff,
                            last_error.take().expect("set above"),
                        )
                        .await?;
                        continue;
                    }
                    Ok(Err(error)) => return Err(error),
                    Err(_) => {
                        return Err(LlmError::Provider("interrupted by user".to_string()));
                    }
                };
                match serde_json::from_slice::<Value>(&response_bytes) {
                    Ok(value) => {
                        raw = Some(value);
                        break;
                    }
                    Err(error) => {
                        let preview = String::from_utf8_lossy(if response_bytes.len() > 500 {
                            &response_bytes[..500]
                        } else {
                            &response_bytes
                        });
                        warn!(
                            "provider: attempt {}/{} json parse failed ({status:#}): {error}\n  body: {preview}",
                            attempt + 1,
                            max_attempts,
                        );
                        attempt += 1;
                        if attempt >= max_attempts {
                            return Err(LlmError::InvalidResponse(format!(
                                "JSON parse failed after {max_attempts} attempts: {error}"
                            )));
                        }
                        apply_retry_delay(
                            &self.cancel_token,
                            policy,
                            attempt - 1,
                            None,
                            max_backoff,
                            LlmError::InvalidResponse(format!("JSON parse failed: {error}")),
                        )
                        .await?;
                        continue;
                    }
                }
            }

            let status_code = status.as_u16();
            let retry_after = parse_retry_after(response.headers());
            let response_bytes =
                match cancelable(&self.cancel_token, read_provider_response_body(response)).await {
                    Ok(Ok(b)) => b,
                    Ok(Err(LlmError::Http(error))) => {
                        last_error = Some(LlmError::Http(error));
                        attempt += 1;
                        if attempt >= max_attempts {
                            return Err(last_error.expect("set above"));
                        }
                        apply_retry_delay(
                            &self.cancel_token,
                            policy,
                            attempt - 1,
                            None,
                            max_backoff,
                            last_error.take().expect("set above"),
                        )
                        .await?;
                        continue;
                    }
                    Ok(Err(error)) => return Err(error),
                    Err(_) => {
                        return Err(LlmError::Provider("interrupted by user".to_string()));
                    }
                };
            let response_body = provider_error_body_preview(&response_bytes);

            // Auto-detect unsupported response_format: 400 with response_format
            // enabled in Auto mode -> retry once without it. Do NOT count
            // this as a normal attempt.
            if status_code == 400
                && self.config.json_mode == bookforge_core::JsonMode::Auto
                && use_response_format
                && !tried_response_format_fallback
            {
                warn!("provider: response_format unsupported (400), retrying without it");
                self.response_format_supported
                    .store(false, Ordering::Relaxed);
                body.as_object_mut().map(|o| o.remove("response_format"));
                tried_response_format_fallback = true;
                continue; // Does not increment attempt
            }

            last_error = Some(LlmError::HttpStatus {
                status: status_code,
                body: response_body,
            });

            if !is_retryable_status(status_code) {
                return Err(last_error.expect("set above"));
            }
            attempt += 1;
            if attempt >= max_attempts {
                return Err(last_error.expect("set above"));
            }

            apply_retry_delay(
                &self.cancel_token,
                policy,
                attempt - 1,
                retry_after,
                max_backoff,
                last_error.take().expect("set above"),
            )
            .await?;
        }
        let raw = raw.ok_or_else(|| {
            last_error.unwrap_or_else(|| {
                LlmError::Provider("OpenAI-compatible request did not run".to_string())
            })
        })?;

        let finish_reason = raw
            .pointer("/choices/0/finish_reason")
            .and_then(Value::as_str)
            .map(parse_finish_reason)
            .unwrap_or(FinishReason::Unknown);
        let content = raw
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .ok_or_else(|| LlmError::InvalidResponse(empty_content_diagnosis(&raw, finish_reason)))?
            .to_string();
        let input_tokens = raw.pointer("/usage/prompt_tokens").and_then(Value::as_u64);
        let input_cached_tokens = cached_input_tokens(&raw);
        let output_tokens = billable_output_tokens(&raw);

        // Detect reasoning models from their first response so subsequent
        // requests can use a higher max_output_tokens budget.
        let has_reasoning = raw
            .pointer("/choices/0/message/reasoning_content")
            .and_then(Value::as_str)
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        if has_reasoning {
            self.reasoning_detected.store(true, Ordering::Relaxed);
        }

        Ok(CompletionResponse {
            content,
            input_tokens,
            input_cached_tokens,
            output_tokens,
            finish_reason,
            provider_latency_ms: started.elapsed().as_millis() as u64,
            raw,
        })
    }

    fn is_reasoning(&self) -> bool {
        self.reasoning_detected.load(Ordering::Relaxed)
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_json_response_format: true,
            supports_usage_tokens: true,
        }
    }
}

async fn read_provider_response_body(mut response: reqwest::Response) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PROVIDER_RESPONSE_BODY_BYTES as u64)
    {
        return Err(provider_response_body_too_large());
    }

    let initial_capacity = response
        .content_length()
        .map_or(0, |length| length as usize);
    let mut body = Vec::with_capacity(initial_capacity);
    while let Some(chunk) = response.chunk().await? {
        let Some(new_len) = body.len().checked_add(chunk.len()) else {
            return Err(provider_response_body_too_large());
        };
        if new_len > MAX_PROVIDER_RESPONSE_BODY_BYTES {
            return Err(provider_response_body_too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn provider_response_body_too_large() -> LlmError {
    LlmError::InvalidResponse(format!(
        "provider response body exceeded the {MAX_PROVIDER_RESPONSE_BODY_BYTES}-byte limit"
    ))
}

fn provider_error_body_preview(body: &[u8]) -> String {
    let preview_len = body.len().min(MAX_PROVIDER_ERROR_BODY_BYTES);
    let mut preview = String::from_utf8_lossy(&body[..preview_len]).into_owned();
    if body.len() > preview_len {
        preview.push_str("\u{2026} [truncated]");
    }
    preview
}

async fn cancelable<T>(
    token: &CancellationToken,
    fut: impl std::future::Future<Output = T>,
) -> Result<T> {
    tokio::select! {
        value = fut => Ok(value),
        _ = token.cancelled() => Err(LlmError::Provider(
            "interrupted by user".to_string()
        )),
    }
}

async fn cancelable_sleep(token: &CancellationToken, duration: Duration) -> Result<()> {
    tokio::select! {
        _ = sleep(duration) => Ok(()),
        _ = token.cancelled() => Err(LlmError::Provider(
            "interrupted by user".to_string()
        )),
    }
}

async fn apply_retry_delay(
    token: &CancellationToken,
    policy: RetryAfterPolicy,
    attempt: usize,
    retry_after: Option<Duration>,
    max_backoff: Duration,
    error: LlmError,
) -> Result<()> {
    match retry_delay(policy, attempt, retry_after, max_backoff) {
        Some(delay) => cancelable_sleep(token, delay).await,
        None => Err(error),
    }
}

fn is_retryable_status(status: u16) -> bool {
    // 408 (Request Timeout) and 425 (Too Early) are transient by
    // specification: both describe timing conditions that a later retry can
    // resolve, unlike the permanent 4xx family around them.
    status == 429 || status == 408 || status == 425 || (500..=599).contains(&status)
}

/// Parse a `Retry-After` header value, honoring both defined forms.
///
/// RFC 7231 allows either `delay-seconds` or an HTTP-date. The seconds form
/// was already handled; the date form was silently dropped before, which made
/// polite servers look like they had sent nothing. A date in the past means
/// "retry now" and yields a zero delay. Both forms are clamped to
/// [`MAX_HONORED_RETRY_AFTER_SECS`]; the clamp is a durability guard against
/// hostile values, not a policy judgment about small hints.
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let raw = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    let raw = raw.trim();
    if let Ok(secs) = raw.parse::<u64>() {
        return Some(Duration::from_secs(secs.min(MAX_HONORED_RETRY_AFTER_SECS)));
    }
    let target_unix = parse_http_date_unix(raw)?;
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(Duration::from_secs(
        target_unix
            .saturating_sub(now_unix)
            .min(MAX_HONORED_RETRY_AFTER_SECS),
    ))
}

/// Parse an HTTP-date into Unix seconds. Supports IMF-fixdate
/// (`Sun, 06 Nov 1994 08:49:37 GMT`) and the obsolete RFC 850 form
/// (`Sunday, 06-Nov-94 08:49:37 GMT`); asctime is rare enough on the wire
/// that falling back to the exponential backoff for it is acceptable.
fn parse_http_date_unix(input: &str) -> Option<u64> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];

    let body = input.split(',').nth(1)?.trim_start();
    let tokens: Vec<&str> = body.split_whitespace().collect();

    // IMF-fixdate carries five tokens ("06 Nov 1994 08:49:37 GMT"); RFC 850
    // folds the date into one hyphenated token ("06-Nov-94 08:49:37 GMT").
    let (day_token, month_token, year_token, time_token) = match tokens.as_slice() {
        [date_token, time_token, zone] if zone.eq_ignore_ascii_case("GMT") => {
            let (day, rest) = date_token.split_once('-')?;
            let (month, year) = rest.split_once('-')?;
            (day, month, year.to_string(), *time_token)
        }
        [day_token, month_token, year_token, time_token, zone]
            if zone.eq_ignore_ascii_case("GMT") =>
        {
            (
                *day_token,
                *month_token,
                (*year_token).to_string(),
                *time_token,
            )
        }
        _ => return None,
    };

    let day: u32 = day_token.parse().ok()?;
    let month_index = MONTHS
        .iter()
        .position(|name| month_token.eq_ignore_ascii_case(name))? as u32
        + 1;
    let year: i64 = match year_token.parse::<i64>() {
        // Two-digit years only appear in the RFC 850 form; the pivot follows
        // the common two-digit-year convention (00–68 => 20xx).
        Ok(short @ 0..=99) if year_token.len() == 2 => {
            if short < 70 {
                2000 + short
            } else {
                1900 + short
            }
        }
        Ok(full) => full,
        Err(_) => return None,
    };
    let mut clock = time_token.split(':');
    let hour: u32 = clock.next()?.parse().ok()?;
    let minute: u32 = clock.next()?.parse().ok()?;
    let second: u32 = clock.next()?.parse().ok()?;
    // Audit LLM-P3d: bound the year like every other field above. Years
    // below 1600 predate any sane HTTP semantics and can wrap the unsigned
    // Unix result negative-side (days-from-civil goes negative); years
    // beyond 9999 are nonsense on the wire and could overflow the day math.
    // Both collapse to `None` — the same rejection style as the other
    // bounds — so a hostile header falls back to exponential backoff.
    if clock.next().is_some()
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
        || !(1600..=9999).contains(&year)
    {
        return None;
    }

    // Days-from-civil (Howard Hinnant's algorithm); Gregorian, leap-safe.
    let shifted_year = year - i64::from(month_index <= 2);
    let era = shifted_year.div_euclid(400);
    let year_of_era = shifted_year - era * 400;
    let month_of_season = if month_index > 2 {
        month_index - 3
    } else {
        month_index + 9
    };
    let day_of_season = (153 * month_of_season + 2) / 5 + day - 1;
    let day_of_era =
        year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + i64::from(day_of_season);
    let days = era * 146_097 + day_of_era - 719_468;

    Some((days * 86_400 + hour as i64 * 3_600 + minute as i64 * 60 + second as i64) as u64)
}

fn is_retryable_http_error(error: &reqwest::Error) -> bool {
    error.is_timeout()
        || error.is_connect()
        || error.is_request()
        || error.is_body()
        || error.is_decode()
}

fn attempt_limit_for_http_error(error: &reqwest::Error, max_attempts: usize) -> usize {
    if error.is_timeout() {
        max_attempts.min(2)
    } else {
        max_attempts
    }
}

pub(crate) fn exponential_delay(attempt: usize) -> Duration {
    let millis: u64 = 500u64.saturating_mul(2u64.saturating_pow(attempt as u32));
    Duration::from_millis(millis.min(60_000))
}

/// Widen `base` into a ±20% window around it and pick a point inside.
///
/// The offset is mixed from wall-clock nanoseconds and the process id on
/// every call instead of derived only from the attempt index. A purely
/// attempt-derived sequence made every concurrent worker compute the *same*
/// delay for the same retry round — a thundering herd re-synchronized onto
/// the exact moment the rate limiter was least able to absorb them
/// (audit LLM-10).
pub(crate) fn apply_jitter(base: Duration, attempt: usize) -> Duration {
    let millis = base.as_millis() as u64;
    if millis < 2 {
        return base;
    }
    let spread = millis / 5;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|now| now.subsec_nanos() as u64)
        .unwrap_or(0);
    let seed = nanos
        ^ (std::process::id() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (attempt as u64).wrapping_mul(1103515245);
    let offset = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    let offset = (offset >> 33) % spread.max(1);
    Duration::from_millis(millis.saturating_sub(spread / 2).saturating_add(offset))
}

fn retry_delay(
    policy: RetryAfterPolicy,
    attempt: usize,
    retry_after: Option<Duration>,
    max_backoff: Duration,
) -> Option<Duration> {
    match policy {
        RetryAfterPolicy::None => None,

        RetryAfterPolicy::RespectHeader => {
            retry_after.or_else(|| Some(exponential_delay(attempt).min(max_backoff)))
        }

        RetryAfterPolicy::Fixed => Some(Duration::from_millis(750).min(max_backoff)),

        // A server-provided hint outranks our own curve even under the
        // jittered policy; when absent the exponential estimate applies.
        // The hint is still honored through its clamp in `parse_retry_after`.
        RetryAfterPolicy::JitteredExponential => {
            let base = retry_after.unwrap_or_else(|| exponential_delay(attempt).min(max_backoff));
            // Re-clamp after jittering so the ±20% window can never push a
            // delay past the operator's ceiling.
            Some(apply_jitter(base, attempt).min(max_backoff))
        }
    }
}

/// Explain a 200 response that carried no message content.
///
/// Almost always this is a reasoning model that spent its entire output budget
/// thinking and had nothing left to answer with — seen three separate times on
/// Kimi K3, in the QA pass, the flag judge, and glossary proposal, each time
/// costing a paid request and surfacing only as a bare parse error. When the
/// evidence points that way, say so and name the remedy; otherwise stay
/// generic rather than mislabelling a genuinely different failure.
fn empty_content_diagnosis(raw: &Value, finish_reason: FinishReason) -> String {
    let reasoning_only = [
        "/choices/0/message/reasoning_content",
        "/choices/0/message/reasoning",
    ]
    .iter()
    .any(|pointer| {
        raw.pointer(pointer)
            .and_then(Value::as_str)
            .is_some_and(|text| !text.is_empty())
    });

    if finish_reason == FinishReason::Length || reasoning_only {
        let used = raw
            .pointer("/usage/completion_tokens")
            .and_then(Value::as_u64)
            .map(|tokens| format!(" after {tokens} output tokens"))
            .unwrap_or_default();
        return format!(
            "the model produced no content{used}: it exhausted its output budget \
             before answering. Raise the output-token limit for this request \
             (--qa-max-output-tokens on `translate` and `glossary propose`, \
             --max-output-tokens on the `judge_flags` example)."
        );
    }

    "OpenAI-compatible response missing choices[0].message.content".to_string()
}

fn parse_finish_reason(value: &str) -> FinishReason {
    match value {
        "stop" => FinishReason::Stop,
        "length" => FinishReason::Length,
        "content_filter" => FinishReason::ContentFilter,
        "tool_calls" => FinishReason::ToolCalls,
        _ => FinishReason::Unknown,
    }
}

/// Heuristic: which model names imply chain-of-thought output budget?
///
/// Two distinct families live here deliberately:
///
/// * Dedicated reasoners (`deepseek-reasoner`-style IDs, the OpenAI
///   o-series) always consume output tokens for hidden reasoning, toggle or
///   no toggle, and keep their unconditional treatment.
/// * DeepSeek V4 chat tiers (`v4-flash`, `v4-pro`) merely **default** to
///   thinking per DeepSeek's published docs (api-docs.deepseek.com,
///   "Models & Pricing" / "Thinking Mode", checked 2026-08-26), toggled via
///   `{"thinking": {"type": ...}}`. For those the caller must confirm
///   thinking wasn't disabled before treating the model as reasoning — see
///   `new_with_cancel` and the BookForge presets that ship
///   `thinking_disabled: true`.
fn model_ships_with_thinking(model: &str) -> bool {
    let lower = model.to_lowercase();
    lower.contains("reasoner")
        || lower.contains("v4-flash")
        || lower.contains("v4-pro")
        || lower.starts_with("o1")
        || lower.starts_with("o3")
        || lower.starts_with("o4")
}

/// Whether the initial classification treats this config as a reasoning
/// deployment. Dedicated reasoners ignore the disable toggle (their budget
/// is spent regardless of whether the suppression parameter lands);
/// default-thinking chat tiers honor it.
fn bootstrapped_reasoning(config: &OpenAiCompatibleConfig) -> bool {
    let lower = config.model.to_lowercase();
    let dedicated = lower.contains("reasoner")
        || lower.starts_with("o1")
        || lower.starts_with("o3")
        || lower.starts_with("o4");
    dedicated || (model_ships_with_thinking(&config.model) && !config.thinking_disabled)
}

fn local_api_key_is_optional(name: &str) -> bool {
    matches!(name, "OLLAMA_API_KEY" | "LLAMACPP_API_KEY")
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookforge_core::RetryAfterPolicy;
    use tokio::time::Duration;

    /// Read one HTTP request off a mock connection: headers plus the declared
    /// body. A single read is not enough under load: requests arrive split
    /// across segments, and closing with unread inbound data makes Windows
    /// answer with RST instead of FIN, which surfaces to clients as decode
    /// errors.
    async fn read_mock_request<S>(stream: &mut S) -> Vec<u8>
    where
        S: tokio::io::AsyncRead + Unpin,
    {
        use tokio::io::AsyncReadExt;
        let mut request = Vec::new();
        let mut scratch = [0u8; 8192];
        loop {
            match stream.read(&mut scratch).await {
                Ok(0) | Err(_) => break,
                Ok(read) => request.extend_from_slice(&scratch[..read]),
            }
            let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n") else {
                continue;
            };
            let headers = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
            let declared = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if request.len() >= header_end + 4 + declared {
                break;
            }
        }
        request
    }

    #[test]
    fn offline_usage_estimate_is_script_aware() {
        assert_eq!(estimate_tokens("矛盾是普遍存在的"), 8);
        assert_eq!(estimate_tokens("The quick brown fox."), 5);
        assert_eq!(estimate_tokens(""), 1);
    }

    fn offline_provider(base_url: &str, model: &str) -> OpenAiCompatibleProvider {
        offline_provider_with_key(base_url, "BOOKFORGE_OFFLINE_TEST_API_KEY", model)
    }

    fn offline_provider_with_key(
        base_url: &str,
        api_key_env: &str,
        model: &str,
    ) -> OpenAiCompatibleProvider {
        OpenAiCompatibleProvider::new(OpenAiCompatibleConfig {
            base_url: base_url.to_string(),
            api_key_env: api_key_env.to_string(),
            model: model.to_string(),
            timeout_seconds: 10,
            provider_max_attempts: 1,
            thinking_disabled: true,
            retry_after_policy: RetryAfterPolicy::None,
            max_backoff_seconds: 1,
            max_idle_per_host: 1,
            json_mode: bookforge_core::JsonMode::PromptOnly,
        })
        .expect("offline provider config should be valid")
    }

    fn offline_request() -> CompletionRequest {
        CompletionRequest {
            system: "translate".to_string(),
            user: "hello".to_string(),
            response_format: ResponseFormat::Json,
            temperature: 0.2,
            max_output_tokens: Some(256),
            metadata: RequestMetadata::default(),
        }
    }

    #[test]
    fn openrouter_request_uses_unified_reasoning_suppression() {
        let provider = offline_provider("https://openrouter.ai/api/v1", "anthropic/claude-opus-5");

        let body = provider.request_body(&offline_request());

        assert_eq!(body["reasoning"], json!({"enabled": false}));
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn openai_request_uses_chat_completions_reasoning_effort() {
        let provider = offline_provider("https://api.openai.com/v1", "gpt-5.6-terra");

        let body = provider.request_body(&offline_request());

        assert_eq!(body["reasoning_effort"], json!("none"));
        assert!(body.get("reasoning").is_none());
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn deepseek_request_uses_documented_thinking_toggle() {
        let provider = offline_provider("https://api.deepseek.com/v1", "deepseek-v4-flash");

        let body = provider.request_body(&offline_request());

        assert_eq!(body["thinking"], json!({"type": "disabled"}));
        assert!(body.get("reasoning").is_none());
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn deepseek_preset_identity_survives_a_base_url_proxy() {
        let provider = offline_provider_with_key(
            "https://gateway.example.test/deepseek/v1",
            "DEEPSEEK_API_KEY",
            "deepseek-v4-flash",
        );

        let body = provider.request_body(&offline_request());

        assert_eq!(body["thinking"], json!({"type": "disabled"}));
        assert!(body.get("reasoning").is_none());
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn recognized_base_url_wins_over_credential_identity() {
        let provider = offline_provider_with_key(
            "https://api.deepseek.com/v1",
            "OPENROUTER_API_KEY",
            "deepseek-v4-flash",
        );

        let body = provider.request_body(&offline_request());

        assert_eq!(body["thinking"], json!({"type": "disabled"}));
        assert!(body.get("reasoning").is_none());
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn unknown_compatible_endpoint_omits_suppression_and_marks_warning_emitted() {
        let provider = offline_provider("https://llm.example.test/v1", "custom-model");

        let body = provider.request_body(&offline_request());

        assert!(body.get("reasoning").is_none());
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("thinking").is_none());
        assert!(
            provider.thinking_warning_emitted.load(Ordering::Relaxed),
            "the unsupported suppression path must emit its warning"
        );
    }

    // ---- Audit LLM-P3c: zero-token budgets never reach the wire ------------

    #[test]
    fn degenerate_zero_token_budget_is_floored_and_flagged() {
        let provider = offline_provider("https://llm.example.test/v1", "custom-model");
        assert!(
            !provider
                .collapsed_budget_warning_emitted
                .load(Ordering::Relaxed)
        );

        let mut request = offline_request();
        request.max_output_tokens = Some(0);
        let body = provider.request_body(&request);

        assert_eq!(body["max_tokens"], json!(1), "the wire value must be 1");
        assert!(
            provider
                .collapsed_budget_warning_emitted
                .load(Ordering::Relaxed),
            "a collapsed budget must surface its plan warning"
        );

        // A healthy budget passes through untouched and emits no warning.
        let healthy_provider = offline_provider("https://llm.example.test/v1", "custom-model");
        let body = healthy_provider.request_body(&offline_request());
        assert_eq!(body["max_tokens"], json!(256));
        assert!(
            !healthy_provider
                .collapsed_budget_warning_emitted
                .load(Ordering::Relaxed)
        );
    }

    /// Pathological case end-to-end: a segment whose plan collapsed to a
    /// one-token budget still completes against the endpoint — floored at
    /// max_tokens=1, warning flagged, and no error/400-chase behavior.
    #[tokio::test]
    async fn collapsed_budget_request_still_proceeds_with_floored_limit() {
        let (response, wire_body, budget_flagged) = retry_transient_transport(|| {
            use tokio::io::AsyncWriteExt;
            async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("test listener should bind");
                let addr = listener.local_addr().unwrap();
                let server_handle = tokio::spawn(async move {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        return None;
                    };
                    let inbound = read_mock_request(&mut stream).await;
                    let _ = stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                    let payload = json!({
                        "choices": [
                            {"message": {"content": "ok"}, "finish_reason": "stop"}
                        ],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                    });
                    let _ = stream.write_all(payload.to_string().as_bytes()).await;
                    let _ = stream.shutdown().await;
                    Some(inbound)
                });

                let provider = OpenAiCompatibleProvider::new(OpenAiCompatibleConfig {
                    base_url: format!("http://{addr}"),
                    // Local providers intentionally permit an absent API key.
                    api_key_env: "OLLAMA_API_KEY".to_string(),
                    model: "test-model".to_string(),
                    timeout_seconds: 10,
                    provider_max_attempts: 1,
                    thinking_disabled: true,
                    retry_after_policy: RetryAfterPolicy::None,
                    max_backoff_seconds: 1,
                    max_idle_per_host: 1,
                    json_mode: bookforge_core::JsonMode::PromptOnly,
                })
                .expect("offline provider config should be valid");
                let mut request = offline_request();
                request.max_output_tokens = Some(0);
                // A floored budget completes normally instead of erroring.
                let response = provider.complete(request).await?;
                let inbound = server_handle
                    .await
                    .expect("mock server joins")
                    .expect("mock server should capture the inbound request");
                let budget_flagged =
                    provider.collapsed_budget_warning_emitted.load(Ordering::Relaxed);
                Ok((response, inbound, budget_flagged))
            }
        })
        .await
        .expect("floored budget scenario should succeed without transport flake");

        // The run proceeded: the endpoint answered, nothing chased a 400.
        assert_eq!(response.content, "ok");
        assert_eq!(response.finish_reason, FinishReason::Stop);
        assert!(
            budget_flagged,
            "the collapsed budget must be surfaced as a plan warning"
        );

        // And the wire actually carried the floored limit, not zero.
        let raw = String::from_utf8_lossy(&wire_body);
        let body_start = raw.find("\r\n\r\n").expect("headers/body split") + 4;
        let parsed: Value =
            serde_json::from_str(raw[body_start..].trim()).expect("JSON request body");
        assert_eq!(parsed["max_tokens"], json!(1));
    }

    #[test]
    fn billable_output_does_not_double_count_reasoning_breakdown() {
        let raw = json!({
            "usage": {
                "prompt_tokens": 50,
                "completion_tokens": 100,
                "completion_tokens_details": {"reasoning_tokens": 70},
                "total_tokens": 150
            }
        });

        assert_eq!(billable_output_tokens(&raw), Some(100));
    }

    #[test]
    fn billable_output_uses_total_when_gateway_reports_visible_completion_only() {
        let raw = json!({
            "usage": {
                "prompt_tokens": 50,
                "completion_tokens": 30,
                "completion_tokens_details": {"reasoning_tokens": 70},
                "total_tokens": 150
            }
        });

        assert_eq!(billable_output_tokens(&raw), Some(100));
    }

    #[test]
    fn billable_output_falls_back_to_reasoning_for_partial_usage() {
        let raw = json!({
            "usage": {
                "completion_tokens_details": {"reasoning_tokens": 70}
            }
        });

        assert_eq!(billable_output_tokens(&raw), Some(70));
    }

    #[test]
    fn empty_content_from_a_truncated_response_names_the_output_cap() {
        let raw = serde_json::json!({
            "choices": [{ "message": {}, "finish_reason": "length" }],
            "usage": { "completion_tokens": 4096 }
        });

        let message = empty_content_diagnosis(&raw, FinishReason::Length);

        assert!(message.contains("exhausted its output budget"), "{message}");
        assert!(message.contains("--qa-max-output-tokens"), "{message}");
        assert!(message.contains("4096"), "{message}");
    }

    #[test]
    fn empty_content_after_reasoning_only_names_the_output_cap() {
        // Kimi K3 returns `stop` while emitting reasoning and no answer.
        let raw = serde_json::json!({
            "choices": [{
                "message": { "reasoning": "thinking at length" },
                "finish_reason": "stop"
            }]
        });

        let message = empty_content_diagnosis(&raw, FinishReason::Stop);

        assert!(message.contains("--qa-max-output-tokens"), "{message}");
    }

    #[test]
    fn empty_content_without_truncation_evidence_stays_generic() {
        let raw = serde_json::json!({
            "choices": [{ "message": {}, "finish_reason": "content_filter" }]
        });

        let message = empty_content_diagnosis(&raw, FinishReason::ContentFilter);

        assert_eq!(
            message,
            "OpenAI-compatible response missing choices[0].message.content"
        );
    }

    #[test]
    fn retry_policy_none_returns_none_delay() {
        let delay = retry_delay(RetryAfterPolicy::None, 0, None, Duration::from_secs(30));
        assert!(
            delay.is_none(),
            "RetryAfterPolicy::None must return None delay"
        );
    }

    #[test]
    fn retry_after_delay_seconds_is_honored_and_capped() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_static("45"),
        );
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(45)));

        let mut hostile = reqwest::header::HeaderMap::new();
        hostile.insert(
            reqwest::header::RETRY_AFTER,
            // 100 hours.
            reqwest::header::HeaderValue::from_static("360000"),
        );
        assert_eq!(
            parse_retry_after(&hostile),
            Some(Duration::from_secs(MAX_HONORED_RETRY_AFTER_SECS)),
            "the clamp must bound hostile hints"
        );
    }

    #[test]
    fn retry_after_http_date_is_parsed_not_dropped() {
        // The canonical RFC 7231 example (RFC 2616 §3.3.1 test vector).
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_static("Sun, 06 Nov 1994 08:49:37 GMT"),
        );
        let parsed = parse_retry_after(&headers).expect("HTTP-date must yield a delay");
        assert!(
            parsed <= Duration::from_secs(MAX_HONORED_RETRY_AFTER_SECS),
            "a past date must degrade to an immediate retry, got {parsed:?}"
        );

        let unix =
            parse_http_date_unix("Sun, 06 Nov 1994 08:49:37 GMT").expect("IMF-fixdate must parse");
        assert_eq!(unix, 784_111_777);

        let rfc850 =
            parse_http_date_unix("Sunday, 06-Nov-94 08:49:37 GMT").expect("RFC 850 must parse");
        assert_eq!(rfc850, 784_111_777);

        assert_eq!(
            parse_http_date_unix("Sat, 01 Jan 2000 00:00:00 GMT"),
            Some(946_684_800)
        );
        assert_eq!(parse_http_date_unix("not a date"), None);
    }

    // Audit LLM-P3d: absurd years must be rejected, not run through the
    // civil-calendar math where they underflow the unsigned Unix result
    // (or overflow it). The bounds keep hostile Retry-After headers on the
    // same graceful None path as any other malformed date.
    #[test]
    fn http_dates_outside_the_plausible_year_window_are_rejected() {
        assert_eq!(
            parse_http_date_unix("Wed, 01 Jan 1599 00:00:00 GMT"),
            None,
            "pre-1600 years must be rejected"
        );
        assert_eq!(
            parse_http_date_unix("Wed, 01 Jan 10000 00:00:00 GMT"),
            None,
            "five-digit years must be rejected"
        );
        assert_eq!(
            parse_http_date_unix("Wed, 01 Jan -0500 00:00:00 GMT"),
            None,
            "negative years must be rejected before the unsigned cast"
        );
        // Boundary values inside the window still parse.
        assert!(
            parse_http_date_unix("Wed, 01 Jan 1600 00:00:00 GMT").is_some(),
            "the lower bound itself is accepted"
        );
        assert!(
            parse_http_date_unix("Sun, 01 Jan 9999 12:00:00 GMT").is_some(),
            "the upper bound itself is accepted"
        );
    }

    #[test]
    fn jittered_exponential_prefers_a_server_hint() {
        let hint = Some(Duration::from_secs(3));
        for attempt in 0..5 {
            let delay = retry_delay(
                RetryAfterPolicy::JitteredExponential,
                attempt,
                hint,
                Duration::from_secs(30),
            )
            .expect("policy yields a delay");
            // Jitter widens to ±20%, so a honored 3s hint stays near it
            // rather than collapsing onto our own curve.
            assert!(
                delay >= Duration::from_millis(2_000) && delay <= Duration::from_millis(4_000),
                "hint should dominate the schedule, got {delay:?}"
            );
        }
    }

    #[test]
    fn local_provider_keys_are_optional_only_for_known_presets() {
        assert!(local_api_key_is_optional("OLLAMA_API_KEY"));
        assert!(local_api_key_is_optional("LLAMACPP_API_KEY"));
        assert!(!local_api_key_is_optional("OPENAI_API_KEY"));
    }

    #[test]
    fn retry_policy_fixed_returns_750ms() {
        let delay = retry_delay(RetryAfterPolicy::Fixed, 0, None, Duration::from_secs(30));
        assert_eq!(delay, Some(Duration::from_millis(750)));
    }

    #[test]
    fn retry_policy_caps_to_max_backoff() {
        let delay = retry_delay(
            RetryAfterPolicy::JitteredExponential,
            20, // large attempt index yields huge exponential delay
            None,
            Duration::from_secs(2),
        );
        if let Some(d) = delay {
            assert!(
                d <= Duration::from_secs(2),
                "delay {d:?} must be capped at 2s"
            );
        }
    }

    #[tokio::test]
    async fn retry_policy_none_apply_retry_delay_returns_error() {
        let token = CancellationToken::new();
        let result = apply_retry_delay(
            &token,
            RetryAfterPolicy::None,
            0,
            None,
            Duration::from_secs(30),
            LlmError::Provider("test error".to_string()),
        )
        .await;
        assert!(result.is_err(), "None policy must return error, not sleep");
    }

    #[tokio::test]
    async fn cancel_token_aborts_cancelable() {
        let token = CancellationToken::new();
        token.cancel();

        let result = cancelable(&token, std::future::pending::<()>()).await;
        assert!(result.is_err(), "cancelled token must abort cancelable");
    }

    #[tokio::test]
    async fn cancel_token_aborts_cancelable_sleep() {
        let token = CancellationToken::new();
        token.cancel();

        let result = cancelable_sleep(&token, Duration::from_secs(3600)).await;
        assert!(result.is_err(), "cancelled token must abort sleep");
    }

    #[test]
    fn provider_error_body_preview_is_truncated() {
        let body = vec![b'x'; MAX_PROVIDER_ERROR_BODY_BYTES + 1];
        let preview = provider_error_body_preview(&body);

        assert!(preview.ends_with("[truncated]"));
        assert!(!preview.contains(&"x".repeat(MAX_PROVIDER_ERROR_BODY_BYTES + 1)));
    }

    /// Regression for audit LLM-13. DeepSeek's published docs state that
    /// `deepseek-v4-flash` (and `-pro`) support both modes with thinking
    /// enabled *by default* — but every BookForge preset ships
    /// `thinking_disabled: true`, and a disabled request never burns its
    /// output budget on hidden chain-of-thought, so it must not receive the
    /// reasoning ×3 budget or the ≥300s timeout floor.
    #[test]
    fn thinking_disabled_names_do_not_bootstrap_reasoning_budgets() {
        let disabled = offline_provider("https://api.deepseek.com/v1", "deepseek-v4-flash");
        assert!(
            !disabled.is_reasoning(),
            "thinking_disabled flash must not pre-classify as reasoning"
        );
        let disabled_pro = offline_provider("https://api.deepseek.com/v1", "deepseek-v4-pro");
        assert!(!disabled_pro.is_reasoning());

        // With thinking allowed, the documented default applies.
        let mut enabled_config =
            OpenAiCompatibleConfig::deepseek(Some("deepseek-v4-flash".to_string()));
        enabled_config.thinking_disabled = false;
        enabled_config.base_url = "https://api.deepseek.test/v1".to_string();
        enabled_config.timeout_seconds = 10;
        enabled_config.api_key_env = "BOOKFORGE_OFFLINE_TEST_API_KEY".to_string();
        let enabled = OpenAiCompatibleProvider::new(enabled_config).expect("provider");
        assert!(
            enabled.is_reasoning(),
            "a thinking-enabled V4 chat model defaults to thinking per provider docs"
        );
    }

    #[test]
    fn dedicated_reasoner_ids_stay_classified_as_reasoning() {
        let reasoner = offline_provider("https://api.deepseek.com/v1", "deepseek-reasoner");
        assert!(
            reasoner.is_reasoning(),
            "deepseek-reasoner-style IDs remain reasoning regardless of the toggle"
        );
    }

    #[test]
    fn transient_timing_statuses_are_retryable() {
        assert!(is_retryable_status(408));
        assert!(is_retryable_status(425));
        assert!(is_retryable_status(429));
        assert!(is_retryable_status(503));
        assert!(!is_retryable_status(400));
        assert!(!is_retryable_status(404));
    }

    /// Windows loopback connections intermittently reset or truncate under
    /// heavy thread churn, and a fast mock server can finish streaming before
    /// a loaded client trips its own byte limit (surfacing as a chunk-decode
    /// error instead of the limit error). These are environmental, so
    /// transport-level failures get a few whole-scenario retries before the
    /// test treats them as a real regression.
    fn is_transient_transport_error(error: &LlmError) -> bool {
        let mut detail = error.to_string().to_ascii_lowercase();
        let mut source = std::error::Error::source(error);
        while let Some(error) = source {
            detail.push_str("; ");
            detail.push_str(&error.to_string().to_ascii_lowercase());
            if let Some(io_error) = error.downcast_ref::<std::io::Error>() {
                detail.push_str(&format!(
                    " os-code={}",
                    io_error.raw_os_error().unwrap_or(0)
                ));
            }
            source = error.source();
        }
        // reqwest hides the OS code behind Debug-only formatting, so also
        // scan the debug representation for the well-known Winsock codes.
        detail.push_str(&format!("{error:?}").to_ascii_lowercase());
        detail.contains("connection reset")
            || detail.contains("forcibly closed")
            || detail.contains("os-code=10054")
            || detail.contains("broken pipe")
            || detail.contains("10038")
            || detail.contains("error decoding response body")
    }

    async fn retry_transient_transport<T, F, Fut>(
        mut attempt: F,
    ) -> std::result::Result<T, LlmError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = std::result::Result<T, LlmError>>,
    {
        let mut last = None;
        for index in 0..5 {
            if index > 0 {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            match attempt().await {
                Ok(value) => return Ok(value),
                Err(error) if is_transient_transport_error(&error) => last = Some(error),
                Err(error) => return Err(error),
            }
        }
        Err(last.expect("a retried transport error must be recorded"))
    }

    #[tokio::test]
    async fn oversized_provider_response_body_is_rejected_while_streaming() {
        // Each scenario attempt gets its own listener and one-shot server so
        // a retried attempt never connects to an already-consumed server.
        let error = retry_transient_transport(|| {
            async move {
                use tokio::io::AsyncWriteExt;
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("test listener should bind");
                let addr = listener.local_addr().unwrap();
                let server_handle = tokio::spawn(async move {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        return;
                    };
                    let _ = read_mock_request(&mut stream).await;
                    let _ = stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                        )
                        .await;

                    let chunk = vec![b'x'; 64 * 1024];
                    let chunk_header = format!("{:X}\r\n", chunk.len());
                    for _ in 0..=(MAX_PROVIDER_RESPONSE_BODY_BYTES / chunk.len()) {
                        if stream.write_all(chunk_header.as_bytes()).await.is_err()
                            || stream.write_all(&chunk).await.is_err()
                            || stream.write_all(b"\r\n").await.is_err()
                        {
                            break;
                        }
                    }
                    let _ = stream.write_all(b"0\r\n\r\n").await;
                    let _ = stream.shutdown().await;
                });

                let provider = OpenAiCompatibleProvider::new(OpenAiCompatibleConfig {
                    base_url: format!("http://{addr}"),
                    // Local providers intentionally permit an absent API key.
                    api_key_env: "OLLAMA_API_KEY".to_string(),
                    model: "test-model".to_string(),
                    timeout_seconds: 10,
                    provider_max_attempts: 1,
                    thinking_disabled: true,
                    retry_after_policy: RetryAfterPolicy::None,
                    max_backoff_seconds: 1,
                    max_idle_per_host: 1,
                    json_mode: bookforge_core::JsonMode::PromptOnly,
                })
                .unwrap();
                let outcome = provider
                    .complete(CompletionRequest {
                        system: "translate".to_string(),
                        user: "hello".to_string(),
                        response_format: ResponseFormat::Json,
                        temperature: 0.2,
                        max_output_tokens: Some(256),
                        metadata: RequestMetadata::default(),
                    })
                    .await;
                server_handle.abort();
                outcome
            }
        })
        .await
        .expect_err("oversized response must be rejected");

        assert!(
            error.to_string().contains(&format!(
                "exceeded the {MAX_PROVIDER_RESPONSE_BODY_BYTES}-byte limit"
            )),
            "unexpected error: {error}"
        );
    }

    /// Verify that json_mode_auto_fallback retries without response_format
    /// when the server returns 400, and does NOT consume a provider attempt.
    #[tokio::test]
    async fn json_mode_auto_fallback_works_with_one_provider_attempt() {
        // We need to override reading of the API key env var.
        // Use a well-known env var name; the actual value is unused by
        // the test server, but the provider MUST be able to read it.
        unsafe { std::env::set_var("BOOKFORGE_TEST_JSON_FALLBACK_KEY", "test") };

        // Each scenario attempt gets its own listener, request counter, and
        // provider so a retried attempt always observes the full
        // 400-then-200 sequence instead of continuing a previous attempt's
        // server-side state.
        let (_response, fallback_disabled, received) = retry_transient_transport(|| {
            let request_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            async move {
                use tokio::io::AsyncWriteExt;
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("test listener should bind");
                let port = listener.local_addr().unwrap().port();

                // Server: returns 400 on the first request (simulating
                // unsupported response_format), then 200 with valid JSON.
                let server_count = request_count.clone();
                let server_handle = tokio::spawn(async move {
                    loop {
                        let Ok((mut stream, _)) = listener.accept().await else {
                            break;
                        };
                        let cnt =
                            server_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        // Read the request so the client doesn't stall
                        let _ = read_mock_request(&mut stream).await;

                        if cnt == 0 {
                            // First attempt: 400 — unsupported response_format
                            let body =
                                br#"{"error":{"message":"response_format is not supported"}}"#;
                            let header = format!(
                                "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            let _ = stream.write_all(header.as_bytes()).await;
                            let _ = stream.write_all(body).await;
                        } else {
                            // Second attempt: 200 OK with valid translation
                            let body = br#"{"choices":[{"message":{"content":"{\"translation\":\"Ciao\"}"},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":3}}"#;
                            let header = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            let _ = stream.write_all(header.as_bytes()).await;
                            let _ = stream.write_all(body).await;
                        }
                        let _ = stream.shutdown().await;
                    }
                });

                let provider = OpenAiCompatibleProvider::new(OpenAiCompatibleConfig {
                    base_url: format!("http://127.0.0.1:{port}"),
                    api_key_env: "BOOKFORGE_TEST_JSON_FALLBACK_KEY".to_string(),
                    model: "test-model".to_string(),
                    timeout_seconds: 10,
                    provider_max_attempts: 1,
                    thinking_disabled: true,
                    retry_after_policy: RetryAfterPolicy::None,
                    max_backoff_seconds: 30,
                    max_idle_per_host: 32,
                    json_mode: bookforge_core::JsonMode::Auto,
                })
                .unwrap();

                let outcome = provider
                    .complete(CompletionRequest {
                        system: "translate".to_string(),
                        user: "hello".to_string(),
                        response_format: ResponseFormat::Json,
                        temperature: 0.2,
                        max_output_tokens: Some(256),
                        metadata: RequestMetadata::default(),
                    })
                    .await;
                let disabled = !provider
                    .response_format_supported
                    .load(std::sync::atomic::Ordering::Relaxed);
                let received = request_count.load(std::sync::atomic::Ordering::SeqCst);
                server_handle.abort();
                let response = outcome?;
                Ok((response, disabled, received))
            }
        })
        .await
        .expect("json_mode_auto_fallback should succeed after 400 fallback");

        // Server should have received 2 requests (first 400, second 200)
        assert_eq!(
            received, 2,
            "expected 2 requests for 400 fallback + successful retry, got {received}"
        );

        // response_format_supported should be set to false after 400
        assert!(
            fallback_disabled,
            "response_format_supported should be false after 400 fallback"
        );
    }

    #[tokio::test]
    async fn request_metadata_freezes_provider_attempts_per_call() {
        use std::sync::{Arc, atomic::AtomicUsize};
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let request_count = Arc::new(AtomicUsize::new(0));
        let listener = match TcpListener::bind("127.0.0.1:0").await {
            Ok(listener) => listener,
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(err) => panic!("test listener should bind: {err}"),
        };
        let addr = listener.local_addr().unwrap();
        let server_count = request_count.clone();
        let server_handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                server_count.fetch_add(1, Ordering::SeqCst);
                let _ = read_mock_request(&mut stream).await;
                let body = br#"{"error":{"message":"retry me"}}"#;
                let header = format!(
                    "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes()).await;
                let _ = stream.write_all(body).await;
                let _ = stream.shutdown().await;
            }
        });

        let provider = OpenAiCompatibleProvider::new(OpenAiCompatibleConfig {
            base_url: format!("http://{addr}"),
            // Local providers intentionally permit an absent API key.
            api_key_env: "OLLAMA_API_KEY".to_string(),
            model: "test-model".to_string(),
            timeout_seconds: 10,
            provider_max_attempts: 6,
            thinking_disabled: true,
            retry_after_policy: RetryAfterPolicy::RespectHeader,
            max_backoff_seconds: 1,
            max_idle_per_host: 4,
            json_mode: bookforge_core::JsonMode::PromptOnly,
        })
        .unwrap();
        let request = |revision, attempts| CompletionRequest {
            system: "translate".to_string(),
            user: "hello".to_string(),
            response_format: ResponseFormat::Json,
            temperature: 0.2,
            max_output_tokens: Some(256),
            metadata: RequestMetadata {
                runtime_config_revision: Some(revision),
                provider_max_attempts: Some(attempts),
                ..RequestMetadata::default()
            },
        };

        provider
            .complete(request(1, 2))
            .await
            .expect_err("the test server always fails");
        assert_eq!(request_count.load(Ordering::SeqCst), 2);

        provider
            .complete(request(2, 4))
            .await
            .expect_err("the test server always fails");
        assert_eq!(
            request_count.load(Ordering::SeqCst),
            6,
            "the later call must use its new frozen attempt limit"
        );

        server_handle.abort();
    }
}
