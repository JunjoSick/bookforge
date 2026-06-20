use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use tokio::time::{Duration, sleep};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use bookforge_core::{RetryAfterPolicy, marker::is_marker_token};

pub type Result<T> = std::result::Result<T, LlmError>;

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
        let segment_id =
            request.metadata.segment_id.clone().ok_or_else(|| {
                LlmError::Provider("mock request is missing segment_id".to_string())
            })?;

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

        let response_segment_id = if self.mode == MockMode::WrongSegmentId {
            "wrong_segment".to_string()
        } else {
            segment_id
        };
        let template = request
            .metadata
            .prompt_template
            .as_deref()
            .unwrap_or("translate_segment");
        let block_ids = &request.metadata.block_ids;

        let content = match template {
            "translate_marker_safe" => {
                let block_sources = extract_block_sources_from_json(&request.user, block_ids);
                let blocks = block_ids
                    .iter()
                    .map(|block_id| {
                        let source = block_sources
                            .get(block_id.as_str())
                            .cloned()
                            .unwrap_or_default();
                        let translated = transform_text(self.mode, &self.target_language, &source);
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
    text.split_whitespace().count().max(1) as u64
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
        let is_reasoning = model_name_is_reasoning(&config.model);
        let effective_timeout = if is_reasoning {
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
            reasoning_detected: AtomicBool::new(is_reasoning),
            response_format_supported: AtomicBool::new(true),
            cancel_token,
        })
    }

    pub fn model(&self) -> &str {
        &self.config.model
    }
}

impl LlmProvider for OpenAiCompatibleProvider {
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
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
        let mut body = json!({
            "model": self.config.model,
            "temperature": request.temperature,
            "messages": [
                {"role": "system", "content": request.system},
                {"role": "user", "content": request.user}
            ]
        });

        if let Some(max_tokens) = request.max_output_tokens {
            body["max_tokens"] = json!(max_tokens);
        }

        if self.config.thinking_disabled {
            body["thinking"] = json!({"type": "disabled"});
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

        let max_attempts = self.config.provider_max_attempts.max(1);
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
                let response_bytes = match cancelable(&self.cancel_token, response.bytes()).await {
                    Ok(Ok(b)) => b,
                    Ok(Err(error)) => {
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
            let response_body = match cancelable(&self.cancel_token, response.text()).await {
                Ok(Ok(b)) => b,
                Ok(Err(e)) => {
                    last_error = Some(LlmError::Http(e));
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
                Err(_) => {
                    return Err(LlmError::Provider("interrupted by user".to_string()));
                }
            };

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

        let content = raw
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                LlmError::InvalidResponse(
                    "OpenAI-compatible response missing choices[0].message.content".to_string(),
                )
            })?
            .to_string();
        let finish_reason = raw
            .pointer("/choices/0/finish_reason")
            .and_then(Value::as_str)
            .map(parse_finish_reason)
            .unwrap_or(FinishReason::Unknown);
        let input_tokens = raw.pointer("/usage/prompt_tokens").and_then(Value::as_u64);
        let input_cached_tokens = cached_input_tokens(&raw);
        let output_tokens = raw
            .pointer("/usage/completion_tokens")
            .and_then(Value::as_u64);

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
    status == 429 || (500..=599).contains(&status)
}

fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let raw = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    let secs: u64 = raw.trim().parse().ok()?;
    // Cap at 60s so a buggy or hostile server can't stall a request for hours.
    Some(Duration::from_secs(secs.min(60)))
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

fn exponential_delay(attempt: usize) -> Duration {
    let millis: u64 = 500u64.saturating_mul(2u64.saturating_pow(attempt as u32));
    Duration::from_millis(millis.min(60_000))
}

fn apply_jitter(base: Duration, attempt: usize) -> Duration {
    let millis = base.as_millis() as u64;
    if millis < 2 {
        return base;
    }
    let spread = millis / 5;
    let offset = (attempt as u64)
        .wrapping_mul(1103515245)
        .wrapping_add(12345)
        % spread.max(1);
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

        RetryAfterPolicy::JitteredExponential => {
            let base = exponential_delay(attempt).min(max_backoff);
            Some(apply_jitter(base, attempt))
        }
    }
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

/// Heuristic to detect reasoning / chain-of-thought models by name.
/// These models consume part of the `max_tokens` budget for internal reasoning
/// and thus need a higher output token allowance.
fn model_name_is_reasoning(model: &str) -> bool {
    let lower = model.to_lowercase();
    lower.contains("reasoner")
        || lower.contains("v4-flash")
        || lower.starts_with("o1")
        || lower.starts_with("o3")
        || lower.starts_with("o4")
}

fn local_api_key_is_optional(name: &str) -> bool {
    matches!(name, "OLLAMA_API_KEY" | "LLAMACPP_API_KEY")
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookforge_core::RetryAfterPolicy;
    use tokio::time::Duration;

    #[test]
    fn retry_policy_none_returns_none_delay() {
        let delay = retry_delay(RetryAfterPolicy::None, 0, None, Duration::from_secs(30));
        assert!(
            delay.is_none(),
            "RetryAfterPolicy::None must return None delay"
        );
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

    /// Verify that json_mode_auto_fallback retries without response_format
    /// when the server returns 400, and does NOT consume a provider attempt.
    #[tokio::test]
    async fn json_mode_auto_fallback_works_with_one_provider_attempt() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        // We need to override reading of the API key env var.
        // Use a well-known env var name; the actual value is unused by
        // the test server, but the provider MUST be able to read it.
        unsafe { std::env::set_var("BOOKFORGE_TEST_JSON_FALLBACK_KEY", "test") };

        let request_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let request_count_clone = request_count.clone();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let port = addr.port();

        // Server: returns 400 on first request (simulating unsupported
        // response_format), then 200 with valid JSON on second request.
        let server_handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let cnt = request_count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                // Read the request so the client doesn't stall
                let mut buf = vec![0u8; 8192];
                let _ = stream.read(&mut buf).await;

                if cnt == 0 {
                    // First attempt: 400 — unsupported response_format
                    let body = br#"{"error":{"message":"response_format is not supported"}}"#;
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

        let config = OpenAiCompatibleConfig {
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
        };

        let provider = OpenAiCompatibleProvider::new(config).unwrap();
        let request = CompletionRequest {
            system: "translate".to_string(),
            user: "hello".to_string(),
            response_format: ResponseFormat::Json,
            temperature: 0.2,
            max_output_tokens: Some(256),
            metadata: RequestMetadata::default(),
        };

        let result = provider.complete(request).await;

        // Server should have received 2 requests (first 400, second 200)
        let received = request_count.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            received, 2,
            "expected 2 requests for 400 fallback + successful retry, got {received}"
        );

        // The single attempt should succeed after fallback
        assert!(
            result.is_ok(),
            "json_mode_auto_fallback should succeed: {:?}",
            result.err()
        );

        // response_format_supported should be set to false after 400
        assert!(
            !provider
                .response_format_supported
                .load(std::sync::atomic::Ordering::Relaxed),
            "response_format_supported should be false after 400 fallback"
        );

        server_handle.abort();
    }
}
