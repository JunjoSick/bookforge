use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::Instant;

pub type Result<T> = std::result::Result<T, LlmError>;

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("provider error: {0}")]
    Provider(String),

    #[error("invalid response: {0}")]
    InvalidResponse(String),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

pub trait LlmProvider: Send + Sync + 'static {
    fn complete(
        &self,
        request: CompletionRequest,
    ) -> impl std::future::Future<Output = Result<CompletionResponse>> + Send;

    fn capabilities(&self) -> ProviderCapabilities;
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
        let translation = match self.mode {
            MockMode::Identity | MockMode::WrongSegmentId => request.user.clone(),
            MockMode::PrefixTarget => format!("[{}] {}", self.target_language, request.user),
            MockMode::Uppercase => request.user.to_uppercase(),
            MockMode::MalformedJson => unreachable!("handled above"),
        };
        let content = serde_json::to_string(&json!({
            "segment_id": response_segment_id,
            "translation": translation,
        }))?;

        Ok(CompletionResponse {
            input_tokens: Some(estimate_tokens(&request.user)),
            output_tokens: Some(estimate_tokens(&content)),
            finish_reason: FinishReason::Stop,
            provider_latency_ms: started.elapsed().as_millis() as u64,
            raw: json!({"provider": "mock", "mode": format!("{:?}", self.mode)}),
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

fn estimate_tokens(text: &str) -> u64 {
    text.split_whitespace().count().max(1) as u64
}
