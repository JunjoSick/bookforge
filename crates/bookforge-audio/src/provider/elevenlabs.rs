use tokio_util::sync::CancellationToken;

use std::{collections::BTreeMap, fmt};

use super::{
    AudioClip, AudioFormat, MAX_AUDIO_RESPONSE_BODY_BYTES, MAX_JSON_RESPONSE_BODY_BYTES, Result,
    SpeechRequest, TtsError, TtsProvider, base_url_is_loopback, build_http_client,
    required_api_key, send_with_retry, validate_audio_payload, validate_path_component,
};

const ELEVENLABS_METADATA_MAX_ATTEMPTS: usize = 2;

struct ApiKey(String);

impl ApiKey {
    fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ApiKey(***)")
    }
}

/// Absolute maximum Unicode characters accepted by an ElevenLabs TTS model.
pub const ELEVENLABS_MAX_INPUT_CHARS: usize = 40_000;

/// ElevenLabs models in BookForge's preferred auto-selection order.
pub const ELEVENLABS_PREFERRED_MODELS: &[&str] = &[
    "eleven_v3",
    "eleven_flash_v2_5",
    "eleven_turbo_v2_5",
    "eleven_multilingual_v2",
];

/// Degradation order used when the preflight cannot reach the models
/// endpoint (AUDIO-3 / DOC-15): cheapest suitable tier first, so a transient
/// network failure fails open to Flash rather than to the most expensive
/// tier. `eleven_v3` is deliberately absent — as the priciest option it must
/// never be chosen by a *degraded* path, and it is additionally excluded by
/// request whenever speed control is needed.
pub const ELEVENLABS_DEGRADED_FALLBACK_ORDER: &[&str] = &[
    "eleven_flash_v2_5",
    "eleven_turbo_v2_5",
    "eleven_multilingual_v2",
];

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ElevenLabsVoice {
    pub voice_id: String,
    pub name: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ElevenLabsSubscription {
    pub character_count: u64,
    pub character_limit: u64,
}

pub async fn fetch_elevenlabs_subscription(
    config: &ElevenLabsTtsConfig,
) -> Result<ElevenLabsSubscription> {
    fetch_elevenlabs_subscription_with_cancel(config, CancellationToken::new()).await
}

/// AUDIO-17: cancellation-safe variant; a cancelled token aborts an
/// in-flight metadata request instead of waiting out the timeout.
pub async fn fetch_elevenlabs_subscription_with_cancel(
    config: &ElevenLabsTtsConfig,
    cancel_token: CancellationToken,
) -> Result<ElevenLabsSubscription> {
    let api_key = if base_url_is_loopback(&config.base_url) {
        std::env::var(&config.api_key_env)
            .ok()
            .filter(|value| !value.trim().is_empty())
    } else {
        Some(required_api_key(&config.api_key_env)?)
    }
    .map(ApiKey::new);
    fetch_elevenlabs_subscription_request(
        &config.base_url,
        api_key.as_ref(),
        config.timeout_seconds,
        config.max_attempts,
        &cancel_token,
    )
    .await
}

/// Fetch ElevenLabs subscription usage with a caller-supplied API key.
///
/// This path never reads or mutates the process environment.
pub async fn fetch_elevenlabs_subscription_with_key(
    base_url: &str,
    api_key: &str,
    timeout_seconds: u64,
) -> Result<ElevenLabsSubscription> {
    fetch_elevenlabs_subscription_request(
        base_url,
        Some(&ApiKey::new(api_key)),
        timeout_seconds,
        ELEVENLABS_METADATA_MAX_ATTEMPTS,
        &CancellationToken::new(),
    )
    .await
}

/// Cancellation-safe explicit-key twin of
/// [`fetch_elevenlabs_subscription_with_key`].
pub async fn fetch_elevenlabs_subscription_with_key_and_cancel(
    base_url: &str,
    api_key: &str,
    timeout_seconds: u64,
    cancel_token: CancellationToken,
) -> Result<ElevenLabsSubscription> {
    fetch_elevenlabs_subscription_request(
        base_url,
        Some(&ApiKey::new(api_key)),
        timeout_seconds,
        ELEVENLABS_METADATA_MAX_ATTEMPTS,
        &cancel_token,
    )
    .await
}

async fn fetch_elevenlabs_subscription_request(
    base_url: &str,
    api_key: Option<&ApiKey>,
    timeout_seconds: u64,
    max_attempts: usize,
    cancel_token: &CancellationToken,
) -> Result<ElevenLabsSubscription> {
    let endpoint = format!("{}/user/subscription", base_url.trim_end_matches('/'));
    let client = build_http_client(timeout_seconds)?;
    let payload = send_with_retry(
        cancel_token,
        max_attempts,
        MAX_JSON_RESPONSE_BODY_BYTES,
        || {
            let mut request = client.get(&endpoint);
            if let Some(api_key) = api_key {
                request = request.header("xi-api-key", api_key.expose());
            }
            request
        },
    )
    .await?;
    serde_json::from_slice(&payload.bytes).map_err(|error| {
        TtsError::Provider(format!(
            "could not parse ElevenLabs subscription response as JSON: {error}"
        ))
    })
}

pub async fn list_elevenlabs_voices(
    base_url: &str,
    api_key: &str,
    timeout_seconds: u64,
) -> Result<Vec<ElevenLabsVoice>> {
    list_elevenlabs_voices_with_cancel(base_url, api_key, timeout_seconds, CancellationToken::new())
        .await
}

/// AUDIO-17: cancellation-safe voices listing.
pub async fn list_elevenlabs_voices_with_cancel(
    base_url: &str,
    api_key: &str,
    timeout_seconds: u64,
    cancel_token: CancellationToken,
) -> Result<Vec<ElevenLabsVoice>> {
    #[derive(serde::Deserialize)]
    struct VoicesResponse {
        voices: Vec<ElevenLabsVoice>,
    }

    let api_key = ApiKey::new(api_key);
    let endpoint = format!("{}/voices", base_url.trim_end_matches('/'));
    let client = build_http_client(timeout_seconds)?;
    let payload = send_with_retry(
        &cancel_token,
        ELEVENLABS_METADATA_MAX_ATTEMPTS,
        MAX_JSON_RESPONSE_BODY_BYTES,
        || client.get(&endpoint).header("xi-api-key", api_key.expose()),
    )
    .await?;
    let response: VoicesResponse = serde_json::from_slice(&payload.bytes).map_err(|error| {
        TtsError::Provider(format!(
            "could not parse ElevenLabs voices response as JSON: {error}"
        ))
    })?;
    Ok(response.voices)
}

/// Return the documented per-request character limit for an ElevenLabs model.
///
/// Unknown models use the conservative long-form default rather than the
/// absolute Flash/Turbo ceiling. ElevenLabs can add models without BookForge
/// knowing their contract yet, and rejecting locally is safer than spending a
/// provider request that cannot succeed.
pub fn elevenlabs_model_max_input_chars(model: &str) -> usize {
    match model {
        "eleven_flash_v2_5" | "eleven_turbo_v2_5" => 40_000,
        "eleven_flash_v2" | "eleven_turbo_v2" => 30_000,
        "eleven_v3" => 5_000,
        "eleven_multilingual_v1" | "eleven_multilingual_v2" => 10_000,
        _ => 10_000,
    }
}

/// Deterministic cheapest-suitable model for a degraded (cannot-reach-
/// preflight) run. A pure function of the same inputs as the full resolver,
/// so resume attempts after a transient outage hash identically to the
/// original run: the model string is what feeds `synthesis_id`, and a stable
/// fallback means a resumed build reuses the previous run's paid chunks.
///
/// Fail-open target per DOC-15: the CHEAPEST tier that can still satisfy the
/// request, never `eleven_v3` or any premium tier.
pub fn degraded_elevenlabs_model(
    max_chars: usize,
    needs_speed_control: bool,
) -> Option<&'static str> {
    // Speed control rides on voice_settings, which every non-v3 preferred
    // model accepts; only v3's absence would change anything and it is not
    // in the degradation order anyway.
    let _ = needs_speed_control;
    ELEVENLABS_DEGRADED_FALLBACK_ORDER
        .iter()
        .copied()
        .find(|model| elevenlabs_model_max_input_chars(model) >= max_chars)
}

/// Which model was chosen for synthesis and how. The CLI/dashboard surface
/// this in plan/report output; the boolean plus reason make an otherwise
/// invisible cost downgrade visible to operators.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ElevenLabsModelResolution {
    pub model: String,
    /// True when the models endpoint was unreachable and the cheapest
    /// suitable tier was substituted.
    pub degraded: bool,
    pub reason: Option<String>,
}

/// Select the best available ElevenLabs model for the requested contract.
pub async fn resolve_preferred_elevenlabs_model(
    config: &ElevenLabsTtsConfig,
    max_chars: usize,
    needs_speed_control: bool,
) -> Result<String> {
    resolve_preferred_elevenlabs_model_reported(config, max_chars, needs_speed_control)
        .await
        .map(|resolution| resolution.model)
}

/// Cancellation-safe, fully-reporting resolver. Callers that own a run-level
/// token must use this so a cancel during model preflight aborts instead of
/// silently waiting for retries.
pub async fn resolve_preferred_elevenlabs_model_reported_with_cancel(
    config: &ElevenLabsTtsConfig,
    max_chars: usize,
    needs_speed_control: bool,
    cancel_token: CancellationToken,
) -> Result<ElevenLabsModelResolution> {
    match resolve_preferred_elevenlabs_model_inner(
        config,
        max_chars,
        needs_speed_control,
        &cancel_token,
    )
    .await
    {
        Ok(model) => Ok(ElevenLabsModelResolution {
            model,
            degraded: false,
            reason: None,
        }),
        Err(error @ TtsError::Http(_)) if error.is_transient_transport() => {
            let detail = error.to_string();
            let fallback =
                degraded_elevenlabs_model(max_chars, needs_speed_control).ok_or(error)?;
            Ok(ElevenLabsModelResolution {
                model: fallback.to_string(),
                degraded: true,
                reason: Some(format!(
                    "ElevenLabs model preflight failed transiently ({detail}); degraded to the \
                     cheapest suitable tier {fallback} so cost stays bounded and the choice is \
                     deterministic across resume attempts"
                )),
            })
        }
        Err(error) => Err(error),
    }
}

/// Full-resolution variant of [`resolve_preferred_elevenlabs_model`]:
/// identical selection when reachable, but on a transient transport failure
/// (timeout / connection refused) it fails OPEN to the cheapest suitable
/// tier instead of erroring out or defaulting to the most expensive one.
/// Parse failures and HTTP errors keep failing hard — those mean the
/// account/endpoint state is wrong, not merely flaky, and guessing could
/// bill against a model the caller did not expect.
pub async fn resolve_preferred_elevenlabs_model_reported(
    config: &ElevenLabsTtsConfig,
    max_chars: usize,
    needs_speed_control: bool,
) -> Result<ElevenLabsModelResolution> {
    resolve_preferred_elevenlabs_model_reported_with_cancel(
        config,
        max_chars,
        needs_speed_control,
        CancellationToken::new(),
    )
    .await
}

impl TtsError {
    /// Transport-level transience mirror of the retry policy used by
    /// [`super::send_with_retry`]: timeouts and connect failures are the
    /// exactly-the-network class eligible for open degradation.
    pub fn is_transient_transport(&self) -> bool {
        matches!(self, TtsError::Http(inner) if inner.is_timeout() || inner.is_connect())
    }
}

async fn resolve_preferred_elevenlabs_model_inner(
    config: &ElevenLabsTtsConfig,
    max_chars: usize,
    needs_speed_control: bool,
    cancel_token: &CancellationToken,
) -> Result<String> {
    let endpoint = format!("{}/models", config.base_url.trim_end_matches('/'));
    let api_key = if base_url_is_loopback(&config.base_url) {
        std::env::var(&config.api_key_env)
            .ok()
            .filter(|value| !value.trim().is_empty())
    } else {
        Some(required_api_key(&config.api_key_env)?)
    }
    .map(ApiKey::new);
    let client = build_http_client(config.timeout_seconds)?;
    let payload = send_with_retry(
        cancel_token,
        config.max_attempts.min(ELEVENLABS_METADATA_MAX_ATTEMPTS),
        MAX_JSON_RESPONSE_BODY_BYTES,
        || {
            let mut request = client.get(&endpoint);
            if let Some(api_key) = api_key.as_ref() {
                request = request.header("xi-api-key", api_key.expose());
            }
            request
        },
    )
    .await?;
    let value: serde_json::Value = serde_json::from_slice(&payload.bytes).map_err(|error| {
        TtsError::Provider(format!(
            "could not parse ElevenLabs models response as JSON: {error}"
        ))
    })?;
    let models = match &value {
        serde_json::Value::Array(models) => models,
        serde_json::Value::Object(object) => object
            .get("models")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                TtsError::Provider(
                    "ElevenLabs models response did not contain a models array".to_string(),
                )
            })?,
        _ => {
            return Err(TtsError::Provider(
                "ElevenLabs models response was not an array or object".to_string(),
            ));
        }
    };

    let available_models = models
        .iter()
        .filter_map(|entry| {
            let model_id = entry.get("model_id")?.as_str()?;
            let supports_tts = match entry.get("can_do_text_to_speech") {
                Some(serde_json::Value::Bool(value)) => *value,
                None => true,
                Some(_) => false,
            };
            supports_tts.then_some(model_id)
        })
        .collect::<std::collections::HashSet<_>>();

    ELEVENLABS_PREFERRED_MODELS
        .iter()
        .copied()
        .find(|model| {
            available_models.contains(model)
                && elevenlabs_model_max_input_chars(model) >= max_chars
                && !(needs_speed_control && *model == "eleven_v3")
        })
        .map(str::to_string)
        .ok_or_else(|| {
            TtsError::Provider(format!(
                "no available preferred ElevenLabs model supports max_chars={max_chars}{}",
                if needs_speed_control {
                    " with speed control"
                } else {
                    ""
                }
            ))
        })
}

/// Configuration for ElevenLabs' native text-to-speech endpoint.
#[derive(Debug, Clone)]
pub struct ElevenLabsTtsConfig {
    pub base_url: String,
    pub api_key_env: String,
    pub model: String,
    pub timeout_seconds: u64,
    pub max_attempts: usize,
}

impl ElevenLabsTtsConfig {
    pub fn hosted(model: Option<String>) -> Self {
        Self {
            base_url: "https://api.elevenlabs.io/v1".to_string(),
            api_key_env: "ELEVENLABS_API_KEY".to_string(),
            model: model.unwrap_or_else(|| "eleven_multilingual_v2".to_string()),
            timeout_seconds: 120,
            max_attempts: 4,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ElevenLabsTtsProvider {
    config: ElevenLabsTtsConfig,
    client: reqwest::Client,
    cancel_token: CancellationToken,
}

impl ElevenLabsTtsProvider {
    pub fn new(config: ElevenLabsTtsConfig) -> Result<Self> {
        Self::new_with_cancel(config, CancellationToken::new())
    }

    pub fn new_with_cancel(
        config: ElevenLabsTtsConfig,
        cancel_token: CancellationToken,
    ) -> Result<Self> {
        validate_path_component(&config.model, "ElevenLabs model")?;
        let client = build_http_client(config.timeout_seconds)?;
        Ok(Self {
            config,
            client,
            cancel_token,
        })
    }

    fn endpoint(&self, voice: &str, format: AudioFormat) -> Result<reqwest::Url> {
        validate_path_component(voice, "ElevenLabs voice ID")?;
        let output_format = elevenlabs_output_format(format)?;
        let mut url = reqwest::Url::parse(&format!(
            "{}/text-to-speech/{voice}",
            self.config.base_url.trim_end_matches('/')
        ))
        .map_err(|error| TtsError::Provider(format!("invalid ElevenLabs base URL: {error}")))?;
        url.query_pairs_mut()
            .append_pair("output_format", output_format);
        Ok(url)
    }
}

impl TtsProvider for ElevenLabsTtsProvider {
    async fn synthesize(&self, request: SpeechRequest) -> Result<AudioClip> {
        let input_chars = request.text.chars().count();
        let model_limit = elevenlabs_model_max_input_chars(&self.config.model);
        if input_chars > model_limit {
            return Err(TtsError::Provider(format!(
                "ElevenLabs model {} is limited to {model_limit} characters; received {input_chars}",
                self.config.model
            )));
        }
        let endpoint = self.endpoint(&request.voice, request.format)?;
        let api_key = ApiKey::new(required_api_key(&self.config.api_key_env)?);
        let body = elevenlabs_request_body(&self.config.model, &request);
        let payload = send_with_retry(
            &self.cancel_token,
            self.config.max_attempts,
            MAX_AUDIO_RESPONSE_BODY_BYTES,
            || {
                self.client
                    .post(endpoint.clone())
                    .header("xi-api-key", api_key.expose())
                    .json(&body)
            },
        )
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

fn elevenlabs_output_format(format: AudioFormat) -> Result<&'static str> {
    match format {
        AudioFormat::Mp3 => Ok("mp3_44100_128"),
        AudioFormat::Opus => Ok("opus_48000_128"),
        AudioFormat::Wav => Ok("wav_44100"),
        AudioFormat::Pcm => Ok("pcm_24000"),
        AudioFormat::Aac | AudioFormat::Flac => {
            Err(TtsError::UnsupportedFormat(format.extension()))
        }
    }
}

fn elevenlabs_request_body(model: &str, request: &SpeechRequest) -> serde_json::Value {
    let mut body = serde_json::json!({
        "text": request.text,
        "model_id": model,
    });
    if (request.speed - 1.0).abs() >= f32::EPSILON {
        body["voice_settings"] = serde_json::json!({"speed": request.speed});
    }
    if let Some(previous_text) = &request.previous_text {
        body["previous_text"] = serde_json::json!(previous_text);
    }
    if let Some(next_text) = &request.next_text {
        body["next_text"] = serde_json::json!(next_text);
    }
    if let Some(seed) = request.seed {
        body["seed"] = serde_json::json!(seed);
    }
    if let Some(language_code) = &request.language_code {
        body["language_code"] = serde_json::json!(language_code);
    }
    if let Some(text_normalization) = request.text_normalization {
        body["apply_text_normalization"] = serde_json::json!(text_normalization.as_str());
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::test_support::{
        CAPTURE_WINDOW, one_request_server, one_request_server_with_content_length,
        retry_transient_transport,
    };

    fn request(format: AudioFormat) -> SpeechRequest {
        SpeechRequest {
            text: "toki pona".to_string(),
            voice: "JBFqnCBsd6RMkjVDRZzb".to_string(),
            format,
            speed: 0.9,
            instructions: None,
            ..SpeechRequest::default()
        }
    }

    #[test]
    fn api_key_debug_is_redacted() {
        let secret = "elevenlabs-secret-that-must-not-leak";
        let debug = format!("{:?}", ApiKey::new(secret));
        assert_eq!(debug, "ApiKey(***)");
        assert!(!debug.contains(secret));
    }

    #[test]
    fn native_contract_uses_model_id_and_voice_settings() {
        let body = elevenlabs_request_body("eleven_multilingual_v2", &request(AudioFormat::Mp3));
        assert_eq!(body["text"], "toki pona");
        assert_eq!(body["model_id"], "eleven_multilingual_v2");
        let speed = body["voice_settings"]["speed"].as_f64().unwrap();
        assert!((speed - 0.9).abs() < 0.000_001);
    }

    #[test]
    fn endpoint_contains_voice_path_and_provider_output_format() {
        let provider = ElevenLabsTtsProvider::new(ElevenLabsTtsConfig::hosted(None)).unwrap();
        let endpoint = provider
            .endpoint("JBFqnCBsd6RMkjVDRZzb", AudioFormat::Opus)
            .unwrap();
        assert!(
            endpoint
                .path()
                .ends_with("/text-to-speech/JBFqnCBsd6RMkjVDRZzb")
        );
        assert_eq!(endpoint.query(), Some("output_format=opus_48000_128"));
    }

    #[test]
    fn rejects_formats_elevenlabs_does_not_offer() {
        assert!(matches!(
            elevenlabs_output_format(AudioFormat::Flac),
            Err(TtsError::UnsupportedFormat("flac"))
        ));
        assert!(matches!(
            elevenlabs_output_format(AudioFormat::Aac),
            Err(TtsError::UnsupportedFormat("aac"))
        ));
    }

    #[test]
    fn native_contract_omits_voice_settings_at_default_speed() {
        let mut speech = request(AudioFormat::Mp3);
        speech.speed = 1.0;
        let body = elevenlabs_request_body("eleven_multilingual_v2", &speech);
        assert!(body.get("voice_settings").is_none());
        for field in [
            "previous_text",
            "next_text",
            "seed",
            "language_code",
            "apply_text_normalization",
        ] {
            assert!(body.get(field).is_none(), "unexpected field {field}");
        }
    }

    #[tokio::test]
    async fn synthesis_sends_context_consistency_fields() {
        let raw = retry_transient_transport(|| async {
            let (base_url, captured) = one_request_server(b"ID3mock-audio".to_vec(), "audio/mpeg");
            let key_env = "BOOKFORGE_ELEVENLABS_CONTEXT_BODY_TEST_KEY";
            unsafe { std::env::set_var(key_env, "context-key") };
            let provider = ElevenLabsTtsProvider::new(ElevenLabsTtsConfig {
                base_url,
                api_key_env: key_env.to_string(),
                model: "eleven_flash_v2_5".to_string(),
                timeout_seconds: 5,
                max_attempts: 1,
            })
            .unwrap();
            let mut speech = request(AudioFormat::Mp3);
            speech.previous_text = Some("before".to_string());
            speech.next_text = Some("after".to_string());
            speech.seed = Some(42);
            speech.language_code = Some("it".to_string());
            speech.text_normalization = Some(crate::provider::TextNormalization::On);
            let result = provider.synthesize(speech).await;
            unsafe { std::env::remove_var(key_env) };
            result.map(|_| {
                captured
                    .recv_timeout(CAPTURE_WINDOW)
                    .expect("mock should capture the request")
            })
        })
        .await
        .expect("mocked ElevenLabs synthesis");

        let body: serde_json::Value =
            serde_json::from_str(raw.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["previous_text"], "before");
        assert_eq!(body["next_text"], "after");
        assert_eq!(body["seed"], 42);
        assert_eq!(body["language_code"], "it");
        assert_eq!(body["apply_text_normalization"], "on");
    }

    fn resolver_config(base_url: String, api_key_env: &str) -> ElevenLabsTtsConfig {
        ElevenLabsTtsConfig {
            base_url,
            api_key_env: api_key_env.to_string(),
            model: "unused-by-preflight".to_string(),
            timeout_seconds: 5,
            max_attempts: 1,
        }
    }

    #[tokio::test]
    async fn resolver_picks_v3_when_all_preferred_models_are_available() {
        let body = serde_json::json!({"models": [
            {"model_id": "eleven_multilingual_v2"},
            {"model_id": "eleven_turbo_v2_5"},
            {"model_id": "eleven_flash_v2_5"},
            {"model_id": "eleven_v3"}
        ]});
        let key_env = "BOOKFORGE_ELEVENLABS_RESOLVER_V3_TEST_KEY";
        let resolved = retry_transient_transport(|| {
            let body = body.to_string();
            async move {
                let (base_url, _) = one_request_server(body.into_bytes(), "application/json");
                unsafe { std::env::set_var(key_env, "resolver-v3-key") };
                let resolved = resolve_preferred_elevenlabs_model(
                    &resolver_config(base_url, key_env),
                    5_000,
                    false,
                )
                .await;
                unsafe { std::env::remove_var(key_env) };
                resolved
            }
        })
        .await
        .unwrap();
        assert_eq!(resolved, "eleven_v3");
    }

    #[tokio::test]
    async fn resolver_picks_flash_when_v3_character_limit_is_too_small() {
        let body = serde_json::json!([
            {"model_id": "eleven_v3"},
            {"model_id": "eleven_flash_v2_5"},
            {"model_id": "eleven_turbo_v2_5"},
            {"model_id": "eleven_multilingual_v2"}
        ]);
        let key_env = "BOOKFORGE_ELEVENLABS_RESOLVER_LIMIT_TEST_KEY";
        let resolved = retry_transient_transport(|| {
            let body = body.to_string();
            async move {
                let (base_url, _) = one_request_server(body.into_bytes(), "application/json");
                unsafe { std::env::set_var(key_env, "resolver-limit-key") };
                let resolved = resolve_preferred_elevenlabs_model(
                    &resolver_config(base_url, key_env),
                    8_000,
                    false,
                )
                .await;
                unsafe { std::env::remove_var(key_env) };
                resolved
            }
        })
        .await
        .unwrap();
        assert_eq!(resolved, "eleven_flash_v2_5");
    }

    #[tokio::test]
    async fn resolver_skips_models_without_text_to_speech() {
        let body = serde_json::json!([
            {"model_id": "eleven_v3", "can_do_text_to_speech": false},
            {"model_id": "eleven_flash_v2_5", "can_do_text_to_speech": true}
        ]);
        let key_env = "BOOKFORGE_ELEVENLABS_RESOLVER_TTS_TEST_KEY";
        let resolved = retry_transient_transport(|| {
            let body = body.to_string();
            async move {
                let (base_url, _) = one_request_server(body.into_bytes(), "application/json");
                unsafe { std::env::set_var(key_env, "resolver-tts-key") };
                let resolved = resolve_preferred_elevenlabs_model(
                    &resolver_config(base_url, key_env),
                    5_000,
                    false,
                )
                .await;
                unsafe { std::env::remove_var(key_env) };
                resolved
            }
        })
        .await
        .unwrap();
        assert_eq!(resolved, "eleven_flash_v2_5");
    }

    #[tokio::test]
    async fn resolver_skips_v3_when_speed_control_is_needed() {
        let body = serde_json::json!([
            {"model_id": "eleven_v3"},
            {"model_id": "eleven_flash_v2_5"}
        ]);
        let key_env = "BOOKFORGE_ELEVENLABS_RESOLVER_SPEED_TEST_KEY";
        let resolved = retry_transient_transport(|| {
            let body = body.to_string();
            async move {
                let (base_url, _) = one_request_server(body.into_bytes(), "application/json");
                unsafe { std::env::set_var(key_env, "resolver-speed-key") };
                let resolved = resolve_preferred_elevenlabs_model(
                    &resolver_config(base_url, key_env),
                    5_000,
                    true,
                )
                .await;
                unsafe { std::env::remove_var(key_env) };
                resolved
            }
        })
        .await
        .unwrap();
        assert_eq!(resolved, "eleven_flash_v2_5");
    }

    #[tokio::test]
    async fn resolver_sends_models_get_and_api_key_header() {
        let body = serde_json::json!([{"model_id": "eleven_multilingual_v2"}]);
        let key_env = "BOOKFORGE_ELEVENLABS_RESOLVER_REQUEST_TEST_KEY";
        let (resolved, raw) = retry_transient_transport(|| {
            let body = body.to_string();
            async move {
                let (base_url, captured) =
                    one_request_server(body.into_bytes(), "application/json");
                unsafe { std::env::set_var(key_env, "resolver-request-key") };
                let resolved = resolve_preferred_elevenlabs_model(
                    &resolver_config(base_url, key_env),
                    5_000,
                    false,
                )
                .await;
                unsafe { std::env::remove_var(key_env) };
                resolved.map(|resolved| {
                    let raw = captured
                        .recv_timeout(CAPTURE_WINDOW)
                        .expect("mock should capture the request");
                    (resolved, raw)
                })
            }
        })
        .await
        .unwrap();

        assert_eq!(resolved, "eleven_multilingual_v2");
        assert!(raw.starts_with("GET /v1/models HTTP/1.1"));
        assert!(
            raw.to_ascii_lowercase()
                .contains("xi-api-key: resolver-request-key")
        );
    }

    #[tokio::test]
    async fn resolver_errors_on_unparseable_body() {
        let key_env = "BOOKFORGE_ELEVENLABS_RESOLVER_GARBAGE_TEST_KEY";
        let error = retry_transient_transport(|| async {
            let (base_url, _) = one_request_server(b"not json".to_vec(), "application/json");
            unsafe { std::env::set_var(key_env, "resolver-garbage-key") };
            let resolved = resolve_preferred_elevenlabs_model(
                &resolver_config(base_url, key_env),
                5_000,
                false,
            )
            .await
            .map(|_| panic!("garbage body should not resolve"));
            unsafe { std::env::remove_var(key_env) };
            resolved
        })
        .await
        .unwrap_err();
        assert!(matches!(error, TtsError::Provider(_)));
        assert!(error.to_string().contains("parse ElevenLabs models"));
    }

    #[tokio::test]
    async fn resolver_errors_on_empty_body() {
        let key_env = "BOOKFORGE_ELEVENLABS_RESOLVER_EMPTY_TEST_KEY";
        let error = retry_transient_transport(|| async {
            let (base_url, _) = one_request_server(Vec::new(), "application/json");
            unsafe { std::env::set_var(key_env, "resolver-empty-key") };
            let resolved = resolve_preferred_elevenlabs_model(
                &resolver_config(base_url, key_env),
                5_000,
                false,
            )
            .await
            .map(|_| panic!("empty body should not resolve"));
            unsafe { std::env::remove_var(key_env) };
            resolved
        })
        .await
        .unwrap_err();
        assert!(matches!(error, TtsError::Provider(_)));
        assert!(error.to_string().contains("empty response body"));
    }

    /// Bind + drop a listener to obtain a port that reliably refuses
    /// connections — an offline, deterministic stand-in for a network outage.
    fn closed_loopback_url() -> String {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("listener for port reservation");
        let address = listener.local_addr().expect("reserved address");
        drop(listener);
        format!("http://{address}/v1")
    }

    #[tokio::test]
    async fn unreachable_preflight_fails_open_to_the_cheapest_suitable_tier() {
        let config = resolver_config(closed_loopback_url(), "RESOLVER_DEGRADE_UNUSED_KEY");
        let resolution = resolve_preferred_elevenlabs_model_reported(&config, 5_000, false)
            .await
            .expect("transient outage must degrade, not fail the run");

        assert!(resolution.degraded);
        assert_eq!(resolution.model, "eleven_flash_v2_5");
        assert!(
            resolution
                .reason
                .as_deref()
                .unwrap_or_default()
                .contains("cheapest suitable tier"),
            "{:?}",
            resolution.reason
        );
        // The string-returning wrapper stays contract-compatible.
        assert_eq!(
            resolve_preferred_elevenlabs_model(&config, 5_000, false)
                .await
                .unwrap(),
            "eleven_flash_v2_5"
        );
    }

    #[tokio::test]
    async fn degraded_choice_is_deterministic_across_resume_attempts() {
        let config = resolver_config(closed_loopback_url(), "RESOLVER_DEGRADE_STABLE_KEY");
        let first = resolve_preferred_elevenlabs_model_reported(&config, 12_000, false)
            .await
            .unwrap();
        let second = resolve_preferred_elevenlabs_model_reported(&config, 12_000, false)
            .await
            .unwrap();
        assert_eq!(first, second, "identical inputs must resolve identically");
        // Flash carries the 40k ceiling too, so 12k chars still fit the
        // cheapest tier; turbo would only appear for models that out-live
        // flash's ceiling, which none do today.
        assert_eq!(first.model, "eleven_flash_v2_5");
        assert!(first.degraded && second.degraded);

        // Pure-function parity: same answer without any request at all.
        assert_eq!(
            degraded_elevenlabs_model(12_000, false),
            Some("eleven_flash_v2_5")
        );
    }

    #[test]
    fn degraded_order_never_selects_premium_tiers() {
        for max_chars in [1usize, 5_000, 40_000] {
            if let Some(model) = degraded_elevenlabs_model(max_chars, false) {
                assert_ne!(model, "eleven_v3");
                assert_ne!(model, "eleven_multilingual_v2", "chars={max_chars}");
            }
        }
        assert_eq!(degraded_elevenlabs_model(40_001, false), None);
    }

    #[tokio::test]
    async fn preflight_metadata_honours_cancellation_instead_of_waiting() {
        // AUDIO-17: a cancelled token aborts the request path immediately
        // (before any network work), regardless of endpoint reachability.
        let config = resolver_config(closed_loopback_url(), "RESOLVER_CANCEL_UNUSED_KEY");
        let token = CancellationToken::new();
        token.cancel();
        let started = std::time::Instant::now();
        let outcome =
            resolve_preferred_elevenlabs_model_reported_with_cancel(&config, 5_000, false, token)
                .await;
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        match outcome {
            Err(TtsError::Cancelled) => {}
            other => panic!("expected Cancelled, got {other:?}"),
        }

        let key_token = CancellationToken::new();
        key_token.cancel();
        let cancelled = fetch_elevenlabs_subscription_with_key_and_cancel(
            &closed_loopback_url(),
            "sub-key",
            5,
            key_token,
        )
        .await;
        assert!(matches!(cancelled, Err(TtsError::Cancelled)));
    }

    #[tokio::test]
    async fn explicit_key_subscription_get_parses_counts_and_sends_key() {
        let body = serde_json::json!({"character_count": 123, "character_limit": 456});
        let (subscription, raw) = retry_transient_transport(|| {
            let body = body.to_string();
            async move {
                let (base_url, captured) =
                    one_request_server(body.into_bytes(), "application/json");
                fetch_elevenlabs_subscription_with_key(&base_url, "explicit-subscription-key", 5)
                    .await
                    .map(|subscription| {
                        let raw = captured
                            .recv_timeout(CAPTURE_WINDOW)
                            .expect("mock should capture the request");
                        (subscription, raw)
                    })
            }
        })
        .await
        .unwrap();

        assert_eq!(subscription.character_count, 123);
        assert_eq!(subscription.character_limit, 456);
        assert!(raw.starts_with("GET /v1/user/subscription HTTP/1.1"));
        assert!(
            raw.to_ascii_lowercase()
                .contains("xi-api-key: explicit-subscription-key")
        );
    }

    #[tokio::test]
    async fn config_subscription_wrapper_resolves_key_from_environment() {
        let body = serde_json::json!({"character_count": 123, "character_limit": 456});
        let key_env = "BOOKFORGE_ELEVENLABS_SUBSCRIPTION_WRAPPER_TEST_KEY";
        let (subscription, raw) = retry_transient_transport(|| {
            let body = body.to_string();
            async move {
                let (base_url, captured) =
                    one_request_server(body.into_bytes(), "application/json");
                unsafe { std::env::set_var(key_env, "wrapper-subscription-key") };
                let subscription =
                    fetch_elevenlabs_subscription(&resolver_config(base_url, key_env)).await;
                unsafe { std::env::remove_var(key_env) };
                subscription.map(|subscription| {
                    let raw = captured
                        .recv_timeout(CAPTURE_WINDOW)
                        .expect("mock should capture the request");
                    (subscription, raw)
                })
            }
        })
        .await
        .unwrap();

        assert_eq!(subscription.character_count, 123);
        assert_eq!(subscription.character_limit, 456);
        assert!(raw.starts_with("GET /v1/user/subscription HTTP/1.1"));
        assert!(
            raw.to_ascii_lowercase()
                .contains("xi-api-key: wrapper-subscription-key")
        );
    }

    #[tokio::test]
    async fn config_subscription_wrapper_allows_missing_key_on_loopback() {
        let body = serde_json::json!({"character_count": 12, "character_limit": 34});
        let key_env = "BOOKFORGE_ELEVENLABS_SUBSCRIPTION_LOOPBACK_MISSING_TEST_KEY";
        let (subscription, raw) = retry_transient_transport(|| {
            let body = body.to_string();
            async move {
                let (base_url, captured) =
                    one_request_server(body.into_bytes(), "application/json");
                unsafe { std::env::remove_var(key_env) };
                fetch_elevenlabs_subscription(&resolver_config(base_url, key_env))
                    .await
                    .map(|subscription| {
                        let raw = captured
                            .recv_timeout(CAPTURE_WINDOW)
                            .expect("mock should capture the request");
                        (subscription, raw)
                    })
            }
        })
        .await
        .unwrap();

        assert_eq!(subscription.character_count, 12);
        assert_eq!(subscription.character_limit, 34);
        assert!(!raw.to_ascii_lowercase().contains("xi-api-key:"));
    }

    #[tokio::test]
    async fn subscription_rejects_oversized_json_before_buffering_body() {
        let error = retry_transient_transport(|| async {
            let (base_url, _) = one_request_server_with_content_length(
                b"{}".to_vec(),
                "application/json",
                MAX_JSON_RESPONSE_BODY_BYTES as u64 + 1,
            );
            let outcome: std::result::Result<(), TtsError> =
                fetch_elevenlabs_subscription_with_key(&base_url, "oversize-key", 5)
                    .await
                    .map(|_| panic!("oversized response should be rejected"));
            outcome
        })
        .await
        .unwrap_err();

        assert!(matches!(error, TtsError::Provider(_)));
        assert!(error.to_string().contains("8388608-byte limit"));
    }

    #[tokio::test]
    async fn subscription_errors_on_garbage_body() {
        let key_env = "BOOKFORGE_ELEVENLABS_SUBSCRIPTION_GARBAGE_TEST_KEY";
        let error = retry_transient_transport(|| async {
            let (base_url, _) = one_request_server(b"garbage".to_vec(), "application/json");
            unsafe { std::env::set_var(key_env, "subscription-garbage-key") };
            let subscription = fetch_elevenlabs_subscription(&resolver_config(base_url, key_env))
                .await
                .map(|_| panic!("garbage body should not parse"));
            unsafe { std::env::remove_var(key_env) };
            subscription
        })
        .await
        .unwrap_err();
        assert!(error.to_string().contains("parse ElevenLabs subscription"));
    }

    #[tokio::test]
    async fn voices_get_parses_contract_and_sends_explicit_key() {
        let body = serde_json::json!({"voices": [{
            "voice_id": "voice-1",
            "name": "Narrator",
            "category": "premade",
            "labels": {"accent": "italian"}
        }]});
        let (voices, raw) = retry_transient_transport(|| {
            let body = body.to_string();
            async move {
                let (base_url, captured) =
                    one_request_server(body.into_bytes(), "application/json");
                list_elevenlabs_voices(&base_url, "voices-key", 5)
                    .await
                    .map(|voices| {
                        let raw = captured
                            .recv_timeout(CAPTURE_WINDOW)
                            .expect("mock should capture the request");
                        (voices, raw)
                    })
            }
        })
        .await
        .unwrap();

        assert_eq!(voices.len(), 1);
        assert_eq!(voices[0].voice_id, "voice-1");
        assert_eq!(voices[0].name, "Narrator");
        assert_eq!(voices[0].category, "premade");
        assert_eq!(voices[0].labels["accent"], "italian");
        assert!(raw.starts_with("GET /v1/voices HTTP/1.1"));
        assert!(raw.to_ascii_lowercase().contains("xi-api-key: voices-key"));
    }

    #[tokio::test]
    async fn voices_errors_on_garbage_body() {
        let error = retry_transient_transport(|| async {
            let (base_url, _) = one_request_server(b"garbage".to_vec(), "application/json");
            let outcome: std::result::Result<(), TtsError> =
                list_elevenlabs_voices(&base_url, "voices-garbage-key", 5)
                    .await
                    .map(|_| panic!("garbage body should not parse"));
            outcome
        })
        .await
        .unwrap_err();
        assert!(error.to_string().contains("parse ElevenLabs voices"));
    }

    #[tokio::test]
    async fn rejects_input_above_provider_character_limit_before_network() {
        let provider = ElevenLabsTtsProvider::new(ElevenLabsTtsConfig::hosted(None)).unwrap();
        let mut oversized = request(AudioFormat::Mp3);
        oversized.text = "a".repeat(10_001);

        let error = provider.synthesize(oversized).await.unwrap_err();
        assert!(error.to_string().contains("limited to 10000 characters"));
    }

    #[test]
    fn model_character_limits_match_elevenlabs_contracts() {
        assert_eq!(elevenlabs_model_max_input_chars("eleven_v3"), 5_000);
        assert_eq!(
            elevenlabs_model_max_input_chars("eleven_multilingual_v2"),
            10_000
        );
        assert_eq!(
            elevenlabs_model_max_input_chars("eleven_flash_v2_5"),
            40_000
        );
        assert_eq!(
            elevenlabs_model_max_input_chars("eleven_turbo_v2_5"),
            40_000
        );
        assert_eq!(elevenlabs_model_max_input_chars("future-model"), 10_000);
    }

    #[tokio::test]
    async fn sends_elevenlabs_header_voice_path_query_and_json_to_mock_server() {
        let expected_audio = b"ID3mock-audio".to_vec();
        let key_env = "BOOKFORGE_ELEVENLABS_TTS_CONTRACT_TEST_KEY";
        let (clip_bytes, raw) = retry_transient_transport(|| {
            let expected_audio = expected_audio.clone();
            async move {
                let (base_url, captured) = one_request_server(expected_audio.clone(), "audio/mpeg");
                // SAFETY: this test uses a crate-specific variable that no
                // production code or parallel test reads.
                unsafe { std::env::set_var(key_env, "eleven-test-key") };
                let provider = ElevenLabsTtsProvider::new(ElevenLabsTtsConfig {
                    base_url,
                    api_key_env: key_env.to_string(),
                    model: "eleven_multilingual_v2".to_string(),
                    timeout_seconds: 5,
                    max_attempts: 1,
                })
                .unwrap();
                let clip = provider.synthesize(request(AudioFormat::Mp3)).await;
                unsafe { std::env::remove_var(key_env) };
                clip.map(|clip| {
                    let raw = captured
                        .recv_timeout(CAPTURE_WINDOW)
                        .expect("mock should capture the request");
                    (clip.bytes, raw)
                })
            }
        })
        .await
        .expect("mocked ElevenLabs synthesis");

        assert_eq!(clip_bytes, expected_audio);
        let lowercase = raw.to_ascii_lowercase();
        assert!(raw.starts_with(
            "POST /v1/text-to-speech/JBFqnCBsd6RMkjVDRZzb?output_format=mp3_44100_128 HTTP/1.1"
        ));
        assert!(lowercase.contains("xi-api-key: eleven-test-key"));
        let body: serde_json::Value =
            serde_json::from_str(raw.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["model_id"], "eleven_multilingual_v2");
        assert_eq!(body["text"], "toki pona");
        for field in [
            "previous_text",
            "next_text",
            "seed",
            "language_code",
            "apply_text_normalization",
        ] {
            assert!(body.get(field).is_none(), "unexpected field {field}");
        }
    }
}
