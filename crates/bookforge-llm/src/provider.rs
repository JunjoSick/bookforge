use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use tokio::time::{Duration, sleep};

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

fn is_marker_token(text: &str) -> bool {
    let text = text.trim();
    text == "</m>"
        || text.starts_with("<m ")
        || text.starts_with("<keep ")
        || text.starts_with("<ref ")
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

#[derive(Debug, Clone)]
pub struct OpenAiCompatibleConfig {
    pub base_url: String,
    pub api_key_env: String,
    pub model: String,
    pub timeout_seconds: u64,
    pub provider_max_attempts: usize,
    pub thinking_disabled: bool,
}

impl OpenAiCompatibleConfig {
    pub fn deepseek(model: Option<String>) -> Self {
        Self {
            base_url: "https://api.deepseek.com/v1".to_string(),
            api_key_env: "DEEPSEEK_API_KEY".to_string(),
            model: model.unwrap_or_else(|| "deepseek-chat".to_string()),
            timeout_seconds: 120,
            provider_max_attempts: 6,
            thinking_disabled: false,
        }
    }
}

#[derive(Debug)]
pub struct OpenAiCompatibleProvider {
    config: OpenAiCompatibleConfig,
    client: reqwest::Client,
    reasoning_detected: AtomicBool,
}

impl Clone for OpenAiCompatibleProvider {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            client: self.client.clone(),
            reasoning_detected: AtomicBool::new(self.reasoning_detected.load(Ordering::Relaxed)),
        }
    }
}

impl OpenAiCompatibleProvider {
    pub fn new(config: OpenAiCompatibleConfig) -> Result<Self> {
        let client = reqwest::Client::builder()
            .http1_only()
            .timeout(std::time::Duration::from_secs(config.timeout_seconds))
            .build()?;
        let is_reasoning = model_name_is_reasoning(&config.model);
        Ok(Self {
            config,
            client,
            reasoning_detected: AtomicBool::new(is_reasoning),
        })
    }

    pub fn model(&self) -> &str {
        &self.config.model
    }
}

impl LlmProvider for OpenAiCompatibleProvider {
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        let api_key = std::env::var(&self.config.api_key_env).map_err(|_| {
            LlmError::Provider(format!(
                "environment variable '{}' is not set",
                self.config.api_key_env
            ))
        })?;
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

        if request.response_format == ResponseFormat::Json {
            body["response_format"] = json!({"type": "json_object"});
        }

        let max_attempts = self.config.provider_max_attempts.max(1);
        let body_len = serde_json::to_string(&body).map(|s| s.len()).unwrap_or(0);
        let mut raw = None;
        let mut last_error = None;
        for attempt in 0..max_attempts {
            let response = match self
                .client
                .post(&endpoint)
                .bearer_auth(&api_key)
                .json(&body)
                .send()
                .await
            {
                Ok(response) => response,
                Err(error) => {
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
                    eprintln!(
                        "provider: attempt {}/{} [{kind}] body={body_len}bytes: {error}",
                        attempt + 1,
                        max_attempts,
                    );
                    let retryable = is_retryable_http_error(&error);
                    let attempt_limit = attempt_limit_for_http_error(&error, max_attempts);
                    last_error = Some(LlmError::Http(error));
                    if !retryable || attempt + 1 == attempt_limit {
                        return Err(last_error.expect("set above"));
                    }
                    sleep(backoff_delay(attempt)).await;
                    continue;
                }
            };
            let status = response.status();

            if status.is_success() {
                let response_bytes = match response.bytes().await {
                    Ok(b) => b,
                    Err(error) => {
                        eprintln!(
                            "provider: attempt {}/{} body read failed (status={status:#}): {error}",
                            attempt + 1,
                            max_attempts,
                        );
                        let retryable = is_retryable_http_error(&error);
                        let attempt_limit = attempt_limit_for_http_error(&error, max_attempts);
                        last_error = Some(LlmError::Http(error));
                        if !retryable || attempt + 1 == attempt_limit {
                            return Err(last_error.expect("set above"));
                        }
                        sleep(backoff_delay(attempt)).await;
                        continue;
                    }
                };
                match serde_json::from_slice::<Value>(&response_bytes) {
                    Ok(value) => {
                        raw = Some(value);
                        break;
                    }
                    Err(error) => {
                        let preview = String::from_utf8_lossy(
                            if response_bytes.len() > 500 {
                                &response_bytes[..500]
                            } else {
                                &response_bytes
                            },
                        );
                        eprintln!(
                            "provider: attempt {}/{} json parse failed ({status:#}): {error}\n  body: {preview}",
                            attempt + 1,
                            max_attempts,
                        );
                        if attempt + 1 == max_attempts {
                            return Err(LlmError::InvalidResponse(format!(
                                "JSON parse failed after {max_attempts} attempts: {error}"
                            )));
                        }
                        sleep(backoff_delay(attempt)).await;
                        continue;
                    }
                }
            }

            let status_code = status.as_u16();
            let retry_after = parse_retry_after(response.headers());
            let body = response.text().await.unwrap_or_default();
            last_error = Some(LlmError::HttpStatus {
                status: status_code,
                body,
            });

            if !is_retryable_status(status_code) || attempt + 1 == max_attempts {
                return Err(last_error.expect("set above"));
            }

            sleep(retry_after.unwrap_or_else(|| backoff_delay(attempt))).await;
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

fn backoff_delay(attempt: usize) -> Duration {
    let millis = match attempt {
        0 => 500,
        1 => 1000,
        2 => 3000,
        3 => 8000,
        4 => 20_000,
        _ => 40_000,
    };
    Duration::from_millis(millis)
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
