//! OCR engines used to recover PDF pages whose text reconstruction is weak.

use std::{io::Read, sync::Once, thread, time::Duration};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::{
    Url,
    blocking::{Client, RequestBuilder, Response},
    header::{HeaderMap, RETRY_AFTER},
};
use serde_json::{Value, json};

static UNLIMITED_OCR_NO_PROCESSOR_WARNING: Once = Once::new();

// OCR responses are JSON text, so 8 MiB leaves ample room for dense pages
// while preventing an untrusted endpoint from streaming unbounded data.
const MAX_OCR_RESPONSE_BODY_BYTES: usize = 8 * 1024 * 1024;
const MAX_OCR_ERROR_DETAIL_CHARS: usize = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OcrDialect {
    OpenAiCompatible,
    UnlimitedOcr,
}

#[derive(Debug, Clone)]
pub struct OcrConfig {
    pub base_url: String,
    pub dialect: OcrDialect,
    pub api_key_env: String,
    pub model: String,
    pub prompt: String,
    pub image_mode: String,
    pub ngram_size: u32,
    pub window_size: u32,
    pub logit_processor: Option<String>,
    pub timeout_seconds: u64,
    pub max_attempts: usize,
}

impl OcrConfig {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            dialect: OcrDialect::OpenAiCompatible,
            api_key_env: "OCR_API_KEY".to_string(),
            model: "baidu/Unlimited-OCR".to_string(),
            prompt: "document parsing.".to_string(),
            image_mode: "gundam".to_string(),
            ngram_size: 35,
            window_size: 90,
            logit_processor: None,
            timeout_seconds: 120,
            max_attempts: 3,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OcrError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("OCR provider error: {0}")]
    Provider(String),

    #[error("OCR API key environment variable '{0}' is not set")]
    MissingKey(String),

    #[error("invalid OCR response: {0}")]
    InvalidResponse(String),
}

pub trait OcrEngine: Send + Sync {
    fn ocr_page(&self, image_png: &[u8], page_number: u32) -> Result<String, OcrError>;
}

/// Blocking HTTP OCR client.
///
/// This client MUST NOT run on an async runtime thread because
/// `reqwest::blocking` can panic there. Async callers should use
/// `tokio::task::spawn_blocking`.
#[derive(Clone)]
pub struct HttpOcrClient {
    config: OcrConfig,
    client: Client,
}

impl HttpOcrClient {
    pub fn new(config: OcrConfig) -> Result<Self, OcrError> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(config.timeout_seconds))
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self { config, client })
    }

    pub fn health_check(&self) -> Result<Vec<String>, OcrError> {
        let endpoint = endpoint(&self.config.base_url, "models");
        let api_key = api_key_for_request(&self.config)?;
        let payload = send_with_retry_blocking(self.config.max_attempts, || {
            let request = self.client.get(&endpoint);
            with_api_key(request, api_key.as_deref())
        })?;
        parse_models_response(&payload)
    }
}

impl OcrEngine for HttpOcrClient {
    fn ocr_page(&self, image_png: &[u8], _page_number: u32) -> Result<String, OcrError> {
        let endpoint = endpoint(&self.config.base_url, "chat/completions");
        let body = ocr_request_body(&self.config, image_png);
        let api_key = api_key_for_request(&self.config)?;
        let payload = send_with_retry_blocking(self.config.max_attempts, || {
            let request = self.client.post(&endpoint).json(&body);
            with_api_key(request, api_key.as_deref())
        })?;
        parse_ocr_response(&payload)
    }
}

fn endpoint(base_url: &str, suffix: &str) -> String {
    format!("{}/{}", base_url.trim_end_matches('/'), suffix)
}

fn with_api_key(request: RequestBuilder, api_key: Option<&str>) -> RequestBuilder {
    match api_key {
        Some(key) => request.bearer_auth(key),
        None => request,
    }
}

fn api_key_for_request(config: &OcrConfig) -> Result<Option<String>, OcrError> {
    match std::env::var(&config.api_key_env) {
        Ok(value) if !value.trim().is_empty() => Ok(Some(value)),
        _ if base_url_is_loopback(&config.base_url) => Ok(None),
        _ => Err(OcrError::MissingKey(config.api_key_env.clone())),
    }
}

fn base_url_is_loopback(base_url: &str) -> bool {
    Url::parse(base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .is_some_and(|host| matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1"))
}

fn ocr_request_body(config: &OcrConfig, image_png: &[u8]) -> Value {
    let encoded = STANDARD.encode(image_png);
    let mut body = json!({
        "model": config.model,
        "messages": [{
            "role": "user",
            "content": [
                {
                    "type": "image_url",
                    "image_url": {"url": format!("data:image/png;base64,{encoded}")}
                },
                {"type": "text", "text": config.prompt}
            ]
        }]
    });

    if config.dialect == OcrDialect::UnlimitedOcr {
        body["images_config"] = json!({"image_mode": config.image_mode});
        body["custom_params"] = json!({
            "ngram_size": config.ngram_size,
            "window_size": config.window_size,
        });
        if let Some(processor) = &config.logit_processor {
            body["custom_logit_processor"] = Value::String(processor.clone());
        } else {
            UNLIMITED_OCR_NO_PROCESSOR_WARNING.call_once(|| {
                tracing::warn!(
                    "Unlimited-OCR is configured without a custom logit processor; dense pages may hit repetition loops"
                );
            });
        }
    }

    body
}

fn parse_ocr_response(bytes: &[u8]) -> Result<String, OcrError> {
    let value: Value = serde_json::from_slice(bytes).map_err(|error| {
        OcrError::InvalidResponse(format!("response body is not valid JSON: {error}"))
    })?;
    if let Some(error) = value.get("error") {
        let detail = error
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| error.to_string());
        return Err(OcrError::Provider(truncate_error_detail(&detail)));
    }

    let content = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .ok_or_else(|| {
            OcrError::InvalidResponse("missing choices[0].message.content".to_string())
        })?;

    let text = if let Some(text) = content.as_str() {
        text.to_string()
    } else if let Some(parts) = content.as_array() {
        parts
            .iter()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("")
    } else {
        return Err(OcrError::InvalidResponse(
            "choices[0].message.content must be a string or an array of text parts".to_string(),
        ));
    };
    if text.trim().is_empty() {
        return Err(OcrError::InvalidResponse(
            "choices[0].message.content contains no text".to_string(),
        ));
    }
    Ok(text)
}

fn parse_models_response(bytes: &[u8]) -> Result<Vec<String>, OcrError> {
    let value: Value = serde_json::from_slice(bytes).map_err(|error| {
        OcrError::InvalidResponse(format!("models response is not valid JSON: {error}"))
    })?;
    if let Some(error) = value.get("error") {
        return Err(OcrError::Provider(truncate_error_detail(
            error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown provider error"),
        )));
    }
    let entries = value
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| value.as_array())
        .ok_or_else(|| {
            OcrError::InvalidResponse(
                "models response must be an array or contain a 'data' array".to_string(),
            )
        })?;
    Ok(entries
        .iter()
        .filter_map(|entry| entry.get("id").and_then(Value::as_str))
        .map(str::to_string)
        .collect())
}

fn send_with_retry_blocking<F>(max_attempts: usize, build_request: F) -> Result<Vec<u8>, OcrError>
where
    F: Fn() -> RequestBuilder,
{
    let max_attempts = max_attempts.max(1);
    let mut last_error = None;
    for attempt in 0..max_attempts {
        match build_request().send() {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    let bytes = read_response_body(response)?;
                    if !bytes.is_empty() {
                        return Ok(bytes);
                    }
                    last_error = Some(OcrError::Provider(
                        "provider returned an empty response body".to_string(),
                    ));
                } else {
                    let retryable = status.is_server_error() || status.as_u16() == 429;
                    let retry_after = retry_after_delay(response.headers());
                    let detail = read_response_body(response)?;
                    let detail = truncate_error_detail(&String::from_utf8_lossy(&detail));
                    last_error = Some(OcrError::Provider(format!("HTTP {status}: {detail}")));
                    if !retryable {
                        break;
                    }
                    if attempt + 1 < max_attempts
                        && let Some(delay) = retry_after
                    {
                        thread::sleep(delay);
                        continue;
                    }
                }
            }
            Err(error) => {
                let retryable = error.is_timeout() || error.is_connect();
                last_error = Some(OcrError::Http(error));
                if !retryable {
                    break;
                }
            }
        }

        if attempt + 1 < max_attempts {
            let exponential = 500u64.saturating_mul(1u64 << attempt.min(6));
            let jitter = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| u64::from(duration.subsec_millis()) % 251);
            thread::sleep(Duration::from_millis(exponential.saturating_add(jitter)));
        }
    }
    Err(last_error.unwrap_or_else(|| OcrError::Provider("no attempts were made".to_string())))
}

fn read_response_body(mut response: Response) -> Result<Vec<u8>, OcrError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_OCR_RESPONSE_BODY_BYTES as u64)
    {
        return Err(ocr_response_body_too_large());
    }

    let initial_capacity = response
        .content_length()
        .unwrap_or(0)
        .min(MAX_OCR_RESPONSE_BODY_BYTES.min(64 * 1024) as u64) as usize;
    let mut bytes = Vec::with_capacity(initial_capacity);
    let mut chunk = [0u8; 16 * 1024];
    loop {
        let read = response.read(&mut chunk).map_err(|error| {
            OcrError::Provider(format!("could not read OCR response body: {error}"))
        })?;
        if read == 0 {
            return Ok(bytes);
        }
        if read > MAX_OCR_RESPONSE_BODY_BYTES.saturating_sub(bytes.len()) {
            return Err(ocr_response_body_too_large());
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
}

fn ocr_response_body_too_large() -> OcrError {
    OcrError::Provider(format!(
        "OCR response body exceeds the {MAX_OCR_RESPONSE_BODY_BYTES}-byte limit"
    ))
}

fn truncate_error_detail(detail: &str) -> String {
    detail.chars().take(MAX_OCR_ERROR_DETAIL_CHARS).collect()
}

fn retry_after_delay(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(|seconds| Duration::from_secs(seconds.min(300)))
}

#[cfg(test)]
pub(crate) struct MockOcrEngine {
    result: Result<String, String>,
}

#[cfg(test)]
impl MockOcrEngine {
    pub(crate) fn success(text: impl Into<String>) -> Self {
        Self {
            result: Ok(text.into()),
        }
    }

    pub(crate) fn failure(message: impl Into<String>) -> Self {
        Self {
            result: Err(message.into()),
        }
    }
}

#[cfg(test)]
impl OcrEngine for MockOcrEngine {
    fn ocr_page(&self, _image_png: &[u8], _page_number: u32) -> Result<String, OcrError> {
        self.result.clone().map_err(OcrError::Provider)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::mpsc,
        time::Duration,
    };

    use super::*;

    fn one_request_server(response_body: &str) -> (String, mpsc::Receiver<String>) {
        one_request_server_with_content_length(response_body, response_body.len() as u64)
    }

    fn one_request_server_with_content_length(
        response_body: &str,
        content_length: u64,
    ) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("mock listener");
        let address = listener.local_addr().expect("mock address");
        let response_body = response_body.as_bytes().to_vec();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("mock accept");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("read timeout");
            let mut request = Vec::new();
            let mut scratch = [0u8; 4096];
            loop {
                let read = stream.read(&mut scratch).expect("read request");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&scratch[..read]);
                let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
                let content_length = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if request.len() >= header_end + 4 + content_length {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).expect("headers");
            stream.write_all(&response_body).expect("body");
            sender
                .send(String::from_utf8_lossy(&request).into_owned())
                .expect("captured request");
        });
        (format!("http://{address}/v1"), receiver)
    }

    #[test]
    fn openai_body_has_only_standard_fields() {
        let config = OcrConfig::new("http://localhost:10000/v1");
        let body = ocr_request_body(&config, b"png");
        assert_eq!(body["model"], "baidu/Unlimited-OCR");
        assert_eq!(
            body["messages"][0]["content"][1]["text"],
            "document parsing."
        );
        assert!(
            body["messages"][0]["content"][0]["image_url"]["url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
        assert!(body.get("images_config").is_none());
        assert!(body.get("custom_params").is_none());
        assert!(body.get("custom_logit_processor").is_none());
    }

    #[test]
    fn unlimited_body_adds_dialect_fields_and_optional_processor() {
        let mut config = OcrConfig::new("http://localhost:10000/v1");
        config.dialect = OcrDialect::UnlimitedOcr;
        let without = ocr_request_body(&config, b"png");
        assert_eq!(without["images_config"]["image_mode"], "gundam");
        assert_eq!(without["custom_params"]["ngram_size"], 35);
        assert_eq!(without["custom_params"]["window_size"], 90);
        assert!(without.get("custom_logit_processor").is_none());

        config.logit_processor = Some("processor-blob".to_string());
        let with = ocr_request_body(&config, b"png");
        assert_eq!(with["custom_logit_processor"], "processor-blob");
    }

    #[test]
    fn parses_string_and_text_part_content() {
        assert_eq!(
            parse_ocr_response(br#"{"choices":[{"message":{"content":"hello"}}]}"#).unwrap(),
            "hello"
        );
        assert_eq!(
            parse_ocr_response(
                br#"{"choices":[{"message":{"content":[{"type":"text","text":"hello "},{"type":"image_url","url":"ignored"},{"type":"text","text":"world"}]}}]}"#
            )
            .unwrap(),
            "hello world"
        );
    }

    #[test]
    fn reports_error_bodies_and_missing_choices() {
        let error = parse_ocr_response(br#"{"error":{"message":"bad image"}}"#).unwrap_err();
        assert!(error.to_string().contains("bad image"));
        let missing = parse_ocr_response(br#"{"id":"response"}"#).unwrap_err();
        assert!(missing.to_string().contains("choices[0].message.content"));
    }

    #[test]
    fn tcp_round_trip_uses_path_and_authorization() {
        let (base_url, captured) =
            one_request_server(r#"{"choices":[{"message":{"content":"OCR text"}}]}"#);
        let key_env = "BOOKFORGE_OCR_TEST_ROUND_TRIP_KEY";
        unsafe { std::env::set_var(key_env, "secret") };
        let mut config = OcrConfig::new(base_url);
        config.api_key_env = key_env.to_string();
        config.max_attempts = 1;
        let client = HttpOcrClient::new(config).unwrap();
        assert_eq!(client.ocr_page(b"png", 1).unwrap(), "OCR text");
        unsafe { std::env::remove_var(key_env) };

        let request = captured.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(request.starts_with("POST /v1/chat/completions HTTP/1.1"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer secret")
        );
    }

    #[test]
    fn loopback_succeeds_without_api_key() {
        let (base_url, captured) =
            one_request_server(r#"{"choices":[{"message":{"content":"local OCR"}}]}"#);
        let key_env = "BOOKFORGE_OCR_TEST_UNSET_LOOPBACK_KEY";
        unsafe { std::env::remove_var(key_env) };
        let mut config = OcrConfig::new(base_url);
        config.api_key_env = key_env.to_string();
        config.max_attempts = 1;
        let client = HttpOcrClient::new(config).unwrap();
        assert_eq!(client.ocr_page(b"png", 9).unwrap(), "local OCR");

        let request = captured.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(!request.to_ascii_lowercase().contains("authorization:"));
    }

    #[test]
    fn ocr_page_rejects_oversized_response_before_buffering_body() {
        let body = r#"{"choices":[{"message":{"content":"small"}}]}"#;
        let (base_url, _) =
            one_request_server_with_content_length(body, MAX_OCR_RESPONSE_BODY_BYTES as u64 + 1);
        let key_env = "BOOKFORGE_OCR_TEST_OVERSIZED_PAGE_KEY";
        unsafe { std::env::remove_var(key_env) };
        let mut config = OcrConfig::new(base_url);
        config.api_key_env = key_env.to_string();
        config.max_attempts = 1;
        let client = HttpOcrClient::new(config).unwrap();
        let error = client.ocr_page(b"png", 1).unwrap_err();

        assert!(matches!(error, OcrError::Provider(_)));
        assert!(error.to_string().contains("8388608-byte limit"));
    }

    #[test]
    fn health_check_rejects_oversized_response_before_buffering_body() {
        let (base_url, _) = one_request_server_with_content_length(
            r#"{"data":[]}"#,
            MAX_OCR_RESPONSE_BODY_BYTES as u64 + 1,
        );
        let key_env = "BOOKFORGE_OCR_TEST_OVERSIZED_HEALTH_KEY";
        unsafe { std::env::remove_var(key_env) };
        let mut config = OcrConfig::new(base_url);
        config.api_key_env = key_env.to_string();
        config.max_attempts = 1;
        let client = HttpOcrClient::new(config).unwrap();
        let error = client.health_check().unwrap_err();

        assert!(matches!(error, OcrError::Provider(_)));
        assert!(error.to_string().contains("8388608-byte limit"));
    }
}
