//! Text-to-speech providers.
//!
//! The [`TtsProvider`] trait mirrors the shape of the translation crate's
//! `LlmProvider`: an async `synthesize` plus a little metadata. Two
//! implementations ship here: OpenAI-compatible, Gemini, and ElevenLabs
//! clients, plus [`MockTtsProvider`] for deterministic offline tests.

use std::time::Duration;

use tokio_util::sync::CancellationToken;

mod elevenlabs;
mod gemini;

pub use elevenlabs::{
    ELEVENLABS_MAX_INPUT_CHARS, ELEVENLABS_PREFERRED_MODELS, ElevenLabsSubscription,
    ElevenLabsTtsConfig, ElevenLabsTtsProvider, ElevenLabsVoice, elevenlabs_model_max_input_chars,
    fetch_elevenlabs_subscription, list_elevenlabs_voices, resolve_preferred_elevenlabs_model,
};
pub use gemini::{GeminiTtsConfig, GeminiTtsProvider};

/// Output container/codec requested from the provider. The string form is
/// what OpenAI-compatible endpoints expect in `response_format`, and the
/// extension is used for the files BookForge writes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AudioFormat {
    Mp3,
    Opus,
    Aac,
    Flac,
    #[default]
    Wav,
    Pcm,
}

impl AudioFormat {
    pub fn as_api_str(self) -> &'static str {
        match self {
            AudioFormat::Mp3 => "mp3",
            AudioFormat::Opus => "opus",
            AudioFormat::Aac => "aac",
            AudioFormat::Flac => "flac",
            AudioFormat::Wav => "wav",
            AudioFormat::Pcm => "pcm",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            AudioFormat::Mp3 => "mp3",
            AudioFormat::Opus => "opus",
            AudioFormat::Aac => "aac",
            AudioFormat::Flac => "flac",
            AudioFormat::Wav => "wav",
            AudioFormat::Pcm => "pcm",
        }
    }

    pub fn from_api_str(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "mp3" => Some(AudioFormat::Mp3),
            "opus" => Some(AudioFormat::Opus),
            "aac" => Some(AudioFormat::Aac),
            "flac" => Some(AudioFormat::Flac),
            "wav" => Some(AudioFormat::Wav),
            "pcm" => Some(AudioFormat::Pcm),
            _ => None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TtsError {
    #[error("tts http error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("tts provider error: {0}")]
    Provider(String),

    #[error("tts request was cancelled")]
    Cancelled,

    #[error("tts provider does not support {0} output")]
    UnsupportedFormat(&'static str),
}

pub type Result<T> = std::result::Result<T, TtsError>;

/// A single unit of text to narrate, plus the voice and format it should be
/// rendered in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TextNormalization {
    Auto,
    On,
    Off,
}

impl TextNormalization {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::On => "on",
            Self::Off => "off",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SpeechRequest {
    pub text: String,
    pub voice: String,
    pub format: AudioFormat,
    /// Playback speed multiplier; 1.0 is normal. Providers that ignore it
    /// simply return normal-speed audio.
    pub speed: f32,
    /// Optional delivery and pronunciation guidance. OpenAI's current
    /// `gpt-4o-mini-tts` model supports this; compatible providers may ignore
    /// it.
    pub instructions: Option<String>,
    pub previous_text: Option<String>,
    pub next_text: Option<String>,
    pub seed: Option<u32>,
    pub language_code: Option<String>,
    pub text_normalization: Option<TextNormalization>,
}

/// Rendered audio bytes for one [`SpeechRequest`].
#[derive(Debug, Clone)]
pub struct AudioClip {
    pub bytes: Vec<u8>,
    pub format: AudioFormat,
}

pub trait TtsProvider: Send + Sync {
    fn synthesize(
        &self,
        request: SpeechRequest,
    ) -> impl std::future::Future<Output = Result<AudioClip>> + Send;
}

/// Configuration for an OpenAI-compatible `/audio/speech` endpoint.
#[derive(Debug, Clone)]
pub struct OpenAiTtsConfig {
    pub base_url: String,
    pub api_key_env: String,
    pub model: String,
    pub timeout_seconds: u64,
    pub max_attempts: usize,
}

impl OpenAiTtsConfig {
    /// OpenAI's hosted TTS service defaults.
    pub fn openai(model: Option<String>) -> Self {
        Self {
            base_url: "https://api.openai.com/v1".to_string(),
            api_key_env: "OPENAI_API_KEY".to_string(),
            model: model.unwrap_or_else(|| "gpt-4o-mini-tts".to_string()),
            timeout_seconds: 120,
            max_attempts: 4,
        }
    }
}

#[derive(Debug, Clone)]
pub struct OpenAiTtsProvider {
    config: OpenAiTtsConfig,
    client: reqwest::Client,
    cancel_token: CancellationToken,
}

impl OpenAiTtsProvider {
    pub fn new(config: OpenAiTtsConfig) -> Result<Self> {
        Self::new_with_cancel(config, CancellationToken::new())
    }

    pub fn new_with_cancel(
        config: OpenAiTtsConfig,
        cancel_token: CancellationToken,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(config.timeout_seconds))
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            config,
            client,
            cancel_token,
        })
    }

    fn resolve_api_key(&self) -> Result<Option<String>> {
        match std::env::var(&self.config.api_key_env) {
            Ok(value) if !value.is_empty() => Ok(Some(value)),
            _ if local_api_key_is_optional(&self.config.api_key_env)
                || base_url_is_loopback(&self.config.base_url) =>
            {
                Ok(None)
            }
            _ => Err(TtsError::Provider(format!(
                "environment variable '{}' is not set",
                self.config.api_key_env
            ))),
        }
    }
}

impl TtsProvider for OpenAiTtsProvider {
    async fn synthesize(&self, request: SpeechRequest) -> Result<AudioClip> {
        let api_key = self.resolve_api_key()?;
        let endpoint = format!(
            "{}/audio/speech",
            self.config.base_url.trim_end_matches('/')
        );
        let body = speech_request_body(&self.config.model, &request);

        let payload = send_with_retry(&self.cancel_token, self.config.max_attempts, || {
            let mut builder = self.client.post(&endpoint).json(&body);
            if let Some(key) = api_key.as_deref() {
                builder = builder.bearer_auth(key);
            }
            builder
        })
        .await?;
        validate_audio_payload(
            request.format,
            payload.content_type.as_deref(),
            &payload.bytes,
        )?;
        Ok(AudioClip {
            bytes: payload.bytes,
            format: request.format,
        })
    }
}

pub(super) fn build_http_client(timeout_seconds: u64) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(timeout_seconds))
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_keepalive(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

pub(super) struct HttpPayload {
    pub bytes: Vec<u8>,
    pub content_type: Option<String>,
}

pub(super) fn required_api_key(environment_variable: &str) -> Result<String> {
    match std::env::var(environment_variable) {
        Ok(value) if !value.trim().is_empty() => Ok(value),
        _ => Err(TtsError::Provider(format!(
            "environment variable '{environment_variable}' is not set"
        ))),
    }
}

pub(super) fn validate_path_component(value: &str, label: &str) -> Result<()> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(TtsError::Provider(format!(
            "{label} must contain only letters, digits, '.', '-' or '_'"
        )));
    }
    Ok(())
}

pub(super) async fn send_with_retry<F>(
    cancel_token: &CancellationToken,
    max_attempts: usize,
    build_request: F,
) -> Result<HttpPayload>
where
    F: Fn() -> reqwest::RequestBuilder,
{
    let max_attempts = max_attempts.max(1);
    let mut last_error: Option<TtsError> = None;
    for attempt in 0..max_attempts {
        if cancel_token.is_cancelled() {
            return Err(TtsError::Cancelled);
        }

        let response = tokio::select! {
            _ = cancel_token.cancelled() => return Err(TtsError::Cancelled),
            result = build_request().send() => result,
        };
        match response {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    let content_type = response
                        .headers()
                        .get(reqwest::header::CONTENT_TYPE)
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_string);
                    let bytes = tokio::select! {
                        _ = cancel_token.cancelled() => return Err(TtsError::Cancelled),
                        result = response.bytes() => result?,
                    };
                    if !bytes.is_empty() {
                        return Ok(HttpPayload {
                            bytes: bytes.to_vec(),
                            content_type,
                        });
                    }
                    last_error = Some(TtsError::Provider(
                        "provider returned an empty response body".to_string(),
                    ));
                } else {
                    let retryable = status.is_server_error() || status.as_u16() == 429;
                    let retry_after = retry_after_delay(response.headers());
                    let detail = response.text().await.unwrap_or_default();
                    let detail = detail.chars().take(300).collect::<String>();
                    last_error = Some(TtsError::Provider(format!("HTTP {status}: {detail}")));
                    if !retryable {
                        break;
                    }
                    if attempt + 1 < max_attempts
                        && let Some(delay) = retry_after
                    {
                        tokio::select! {
                            _ = cancel_token.cancelled() => return Err(TtsError::Cancelled),
                            _ = tokio::time::sleep(delay) => {}
                        }
                        continue;
                    }
                }
            }
            Err(error) => {
                let retryable = error.is_timeout() || error.is_connect();
                last_error = Some(TtsError::Http(error));
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
            let backoff = Duration::from_millis(exponential.saturating_add(jitter));
            tokio::select! {
                _ = cancel_token.cancelled() => return Err(TtsError::Cancelled),
                _ = tokio::time::sleep(backoff) => {}
            }
        }
    }
    Err(last_error.unwrap_or_else(|| TtsError::Provider("no attempts were made".to_string())))
}

fn retry_after_delay(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(|seconds| Duration::from_secs(seconds.min(300)))
}

pub(crate) fn validate_audio_payload(
    format: AudioFormat,
    content_type: Option<&str>,
    bytes: &[u8],
) -> Result<()> {
    if bytes.is_empty() {
        return Err(TtsError::Provider(
            "provider returned empty audio".to_string(),
        ));
    }
    if content_type.is_some_and(|value| {
        let value = value.to_ascii_lowercase();
        value.contains("json") || value.contains("html") || value.starts_with("text/")
    }) {
        return Err(TtsError::Provider(format!(
            "provider returned non-audio content type {}",
            content_type.unwrap_or_default()
        )));
    }
    let valid = match format {
        AudioFormat::Mp3 => {
            bytes.starts_with(b"ID3")
                || bytes
                    .get(..2)
                    .is_some_and(|head| head[0] == 0xff && head[1] & 0xe0 == 0xe0)
        }
        AudioFormat::Opus => bytes.starts_with(b"OggS"),
        AudioFormat::Aac => {
            bytes.starts_with(b"ADIF")
                || bytes
                    .get(..2)
                    .is_some_and(|head| head[0] == 0xff && head[1] & 0xf6 == 0xf0)
        }
        AudioFormat::Flac => bytes.starts_with(b"fLaC"),
        AudioFormat::Wav => {
            bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WAVE"
        }
        AudioFormat::Pcm => {
            bytes.len().is_multiple_of(2)
                && content_type.is_none_or(|value| {
                    let value = value.to_ascii_lowercase();
                    value.starts_with("audio/") || value.contains("octet-stream")
                })
        }
    };
    if valid {
        Ok(())
    } else {
        Err(TtsError::Provider(format!(
            "provider returned bytes that are not valid {} audio",
            format.extension()
        )))
    }
}

#[cfg(test)]
pub(super) mod test_support {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::mpsc,
        time::Duration,
    };

    pub fn one_request_server(
        response_body: Vec<u8>,
        content_type: &str,
    ) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("mock listener");
        let address = listener.local_addr().expect("mock address");
        let content_type = content_type.to_string();
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
                let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
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
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response_body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("write headers");
            stream.write_all(&response_body).expect("write response");
            sender
                .send(String::from_utf8_lossy(&request).into_owned())
                .expect("send captured request");
        });
        (format!("http://{address}/v1"), receiver)
    }
}

/// Endpoints where the API key is optional because they run locally.
fn local_api_key_is_optional(name: &str) -> bool {
    matches!(
        name,
        "KOKORO_API_KEY" | "LOCAL_TTS_API_KEY" | "OPENAI_TTS_API_KEY_OPTIONAL"
    )
}

pub(super) fn base_url_is_loopback(base_url: &str) -> bool {
    reqwest::Url::parse(base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .is_some_and(|host| matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1"))
}

fn speech_request_body(model: &str, request: &SpeechRequest) -> serde_json::Value {
    let mut body = serde_json::json!({
        "model": model,
        "input": request.text,
        "voice": request.voice,
        "response_format": request.format.as_api_str(),
        "speed": request.speed,
    });
    if let Some(instructions) = request
        .instructions
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        body["instructions"] = serde_json::Value::String(instructions.to_string());
    }
    body
}

/// A deterministic, offline provider used by tests and `--provider mock`.
/// It emits a valid, silent WAV whose duration scales with the input length
/// so downstream tooling (players, ffmpeg) sees real audio without any
/// network access.
#[derive(Debug, Clone, Default)]
pub struct MockTtsProvider;

impl MockTtsProvider {
    pub fn new() -> Self {
        Self
    }
}

impl TtsProvider for MockTtsProvider {
    async fn synthesize(&self, request: SpeechRequest) -> Result<AudioClip> {
        if request.format != AudioFormat::Wav {
            return Err(TtsError::UnsupportedFormat(request.format.extension()));
        }
        // ~40ms of silence per character, clamped, at 8 kHz mono 16-bit.
        let sample_rate = 8_000u32;
        let millis = (request.text.chars().count() as u32 * 40).clamp(200, 60_000);
        let samples = (sample_rate as u64 * millis as u64 / 1000) as u32;
        let bytes = silent_wav(sample_rate, samples);
        Ok(AudioClip {
            bytes,
            format: AudioFormat::Wav,
        })
    }
}

/// Build a minimal valid PCM WAV (mono, 16-bit) of `samples` silent frames.
fn silent_wav(sample_rate: u32, samples: u32) -> Vec<u8> {
    pcm_s16le_mono_wav(sample_rate, &vec![0; samples as usize * 2])
}

/// Wrap signed 16-bit little-endian mono PCM in a standard RIFF/WAVE header.
pub(super) fn pcm_s16le_mono_wav(sample_rate: u32, pcm: &[u8]) -> Vec<u8> {
    let bits_per_sample: u16 = 16;
    let channels: u16 = 1;
    let byte_rate = sample_rate * u32::from(channels) * u32::from(bits_per_sample) / 8;
    let block_align = channels * bits_per_sample / 8;
    let data_len = u32::try_from(pcm.len()).unwrap_or(u32::MAX);
    let riff_len = 36 + data_len;

    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&riff_len.to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // audio format: PCM
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&bits_per_sample.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(&pcm[..data_len as usize]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_format_round_trips_through_str() {
        for fmt in [
            AudioFormat::Mp3,
            AudioFormat::Opus,
            AudioFormat::Aac,
            AudioFormat::Flac,
            AudioFormat::Wav,
            AudioFormat::Pcm,
        ] {
            assert_eq!(AudioFormat::from_api_str(fmt.as_api_str()), Some(fmt));
        }
        assert_eq!(AudioFormat::from_api_str("m4b"), None);
    }

    #[tokio::test]
    async fn mock_provider_rejects_non_wav_output() {
        let provider = MockTtsProvider::new();
        let clip = provider
            .synthesize(SpeechRequest {
                text: "hello world".to_string(),
                voice: "any".to_string(),
                format: AudioFormat::Mp3,
                speed: 1.0,
                instructions: None,
                ..SpeechRequest::default()
            })
            .await
            .expect_err("mock should reject mislabeled output");
        assert!(matches!(clip, TtsError::UnsupportedFormat("mp3")));
    }

    #[tokio::test]
    async fn mock_provider_emits_valid_nonempty_wav() {
        let provider = MockTtsProvider::new();
        let clip = provider
            .synthesize(SpeechRequest {
                text: "hello world".to_string(),
                voice: "any".to_string(),
                format: AudioFormat::Wav,
                speed: 1.0,
                instructions: None,
                ..SpeechRequest::default()
            })
            .await
            .expect("mock synthesis should succeed");
        assert_eq!(clip.format, AudioFormat::Wav);
        assert!(clip.bytes.len() > 44, "should have header plus samples");
        assert_eq!(&clip.bytes[0..4], b"RIFF");
        assert_eq!(&clip.bytes[8..12], b"WAVE");
    }

    #[tokio::test]
    async fn mock_provider_is_deterministic() {
        let provider = MockTtsProvider::new();
        let request = SpeechRequest {
            text: "same text".to_string(),
            voice: "v".to_string(),
            format: AudioFormat::Wav,
            speed: 1.0,
            instructions: None,
            ..SpeechRequest::default()
        };
        let a = provider.synthesize(request.clone()).await.unwrap();
        let b = provider.synthesize(request).await.unwrap();
        assert_eq!(a.bytes, b.bytes);
    }

    #[test]
    fn openai_body_includes_optional_instructions() {
        let request = SpeechRequest {
            text: "toki".to_string(),
            voice: "alloy".to_string(),
            format: AudioFormat::Mp3,
            speed: 0.9,
            instructions: Some("Pronounce Toki Pona clearly.".to_string()),
            ..SpeechRequest::default()
        };
        let body = speech_request_body("gpt-4o-mini-tts", &request);
        assert_eq!(body["model"], "gpt-4o-mini-tts");
        assert_eq!(body["instructions"], "Pronounce Toki Pona clearly.");
    }

    #[test]
    fn loopback_endpoints_do_not_require_magic_key_names() {
        assert!(base_url_is_loopback("http://localhost:8880/v1"));
        assert!(base_url_is_loopback("http://127.0.0.1:8000/v1"));
        assert!(!base_url_is_loopback("https://api.example.com/v1"));
    }

    #[test]
    fn rejects_successful_http_payloads_that_are_not_audio() {
        let json = br#"{"error":"upstream returned JSON with status 200"}"#;
        let html = b"<!doctype html><title>proxy login</title>";

        assert!(validate_audio_payload(AudioFormat::Mp3, Some("application/json"), json).is_err());
        assert!(validate_audio_payload(AudioFormat::Wav, Some("text/html"), html).is_err());
        assert!(validate_audio_payload(AudioFormat::Mp3, Some("audio/mpeg"), b"not-mp3").is_err());
        assert!(validate_audio_payload(AudioFormat::Wav, Some("audio/wav"), b"RIFFbad").is_err());
    }

    #[test]
    fn accepts_known_audio_signatures() {
        assert!(validate_audio_payload(AudioFormat::Mp3, Some("audio/mpeg"), b"ID3audio").is_ok());
        assert!(
            validate_audio_payload(
                AudioFormat::Wav,
                Some("audio/wav"),
                b"RIFF\0\0\0\0WAVEaudio"
            )
            .is_ok()
        );
    }
}
