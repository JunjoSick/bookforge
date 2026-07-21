use tokio_util::sync::CancellationToken;

use std::collections::BTreeMap;

use super::{
    AudioClip, AudioFormat, Result, SpeechRequest, TtsError, TtsProvider, base_url_is_loopback,
    build_http_client, required_api_key, send_with_retry, validate_audio_payload,
    validate_path_component,
};

/// Absolute maximum Unicode characters accepted by an ElevenLabs TTS model.
pub const ELEVENLABS_MAX_INPUT_CHARS: usize = 40_000;

/// ElevenLabs models in BookForge's preferred auto-selection order.
pub const ELEVENLABS_PREFERRED_MODELS: &[&str] = &[
    "eleven_v3",
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
    let endpoint = format!(
        "{}/user/subscription",
        config.base_url.trim_end_matches('/')
    );
    let api_key = if base_url_is_loopback(&config.base_url) {
        std::env::var(&config.api_key_env)
            .ok()
            .filter(|value| !value.trim().is_empty())
    } else {
        Some(required_api_key(&config.api_key_env)?)
    };
    let client = build_http_client(config.timeout_seconds)?;
    let cancel_token = CancellationToken::new();
    let payload = send_with_retry(&cancel_token, config.max_attempts, || {
        let mut request = client.get(&endpoint);
        if let Some(api_key) = api_key.as_deref() {
            request = request.header("xi-api-key", api_key);
        }
        request
    })
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
    #[derive(serde::Deserialize)]
    struct VoicesResponse {
        voices: Vec<ElevenLabsVoice>,
    }

    let endpoint = format!("{}/voices", base_url.trim_end_matches('/'));
    let client = build_http_client(timeout_seconds)?;
    let cancel_token = CancellationToken::new();
    let payload = send_with_retry(&cancel_token, 2, || {
        client.get(&endpoint).header("xi-api-key", api_key)
    })
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

/// Select the best available ElevenLabs model for the requested contract.
pub async fn resolve_preferred_elevenlabs_model(
    config: &ElevenLabsTtsConfig,
    max_chars: usize,
    needs_speed_control: bool,
) -> Result<String> {
    let endpoint = format!("{}/models", config.base_url.trim_end_matches('/'));
    let api_key = if base_url_is_loopback(&config.base_url) {
        std::env::var(&config.api_key_env)
            .ok()
            .filter(|value| !value.trim().is_empty())
    } else {
        Some(required_api_key(&config.api_key_env)?)
    };
    let client = build_http_client(config.timeout_seconds)?;
    let cancel_token = CancellationToken::new();
    let payload = send_with_retry(&cancel_token, config.max_attempts.min(2), || {
        let mut request = client.get(&endpoint);
        if let Some(api_key) = api_key.as_deref() {
            request = request.header("xi-api-key", api_key);
        }
        request
    })
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
        let api_key = required_api_key(&self.config.api_key_env)?;
        let body = elevenlabs_request_body(&self.config.model, &request);
        let payload = send_with_retry(&self.cancel_token, self.config.max_attempts, || {
            self.client
                .post(endpoint.clone())
                .header("xi-api-key", &api_key)
                .json(&body)
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
    if !((request.speed - 1.0).abs() < f32::EPSILON) {
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
    use crate::provider::test_support::one_request_server;
    use std::time::Duration;

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
        provider.synthesize(speech).await.unwrap();
        unsafe { std::env::remove_var(key_env) };

        let raw = captured.recv_timeout(Duration::from_secs(2)).unwrap();
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
        let (base_url, _) = one_request_server(body.to_string().into_bytes(), "application/json");
        let key_env = "BOOKFORGE_ELEVENLABS_RESOLVER_V3_TEST_KEY";
        unsafe { std::env::set_var(key_env, "resolver-v3-key") };
        let resolved =
            resolve_preferred_elevenlabs_model(&resolver_config(base_url, key_env), 5_000, false)
                .await
                .unwrap();
        unsafe { std::env::remove_var(key_env) };
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
        let (base_url, _) = one_request_server(body.to_string().into_bytes(), "application/json");
        let key_env = "BOOKFORGE_ELEVENLABS_RESOLVER_LIMIT_TEST_KEY";
        unsafe { std::env::set_var(key_env, "resolver-limit-key") };
        let resolved =
            resolve_preferred_elevenlabs_model(&resolver_config(base_url, key_env), 8_000, false)
                .await
                .unwrap();
        unsafe { std::env::remove_var(key_env) };
        assert_eq!(resolved, "eleven_flash_v2_5");
    }

    #[tokio::test]
    async fn resolver_skips_models_without_text_to_speech() {
        let body = serde_json::json!([
            {"model_id": "eleven_v3", "can_do_text_to_speech": false},
            {"model_id": "eleven_flash_v2_5", "can_do_text_to_speech": true}
        ]);
        let (base_url, _) = one_request_server(body.to_string().into_bytes(), "application/json");
        let key_env = "BOOKFORGE_ELEVENLABS_RESOLVER_TTS_TEST_KEY";
        unsafe { std::env::set_var(key_env, "resolver-tts-key") };
        let resolved =
            resolve_preferred_elevenlabs_model(&resolver_config(base_url, key_env), 5_000, false)
                .await
                .unwrap();
        unsafe { std::env::remove_var(key_env) };
        assert_eq!(resolved, "eleven_flash_v2_5");
    }

    #[tokio::test]
    async fn resolver_skips_v3_when_speed_control_is_needed() {
        let body = serde_json::json!([
            {"model_id": "eleven_v3"},
            {"model_id": "eleven_flash_v2_5"}
        ]);
        let (base_url, _) = one_request_server(body.to_string().into_bytes(), "application/json");
        let key_env = "BOOKFORGE_ELEVENLABS_RESOLVER_SPEED_TEST_KEY";
        unsafe { std::env::set_var(key_env, "resolver-speed-key") };
        let resolved =
            resolve_preferred_elevenlabs_model(&resolver_config(base_url, key_env), 5_000, true)
                .await
                .unwrap();
        unsafe { std::env::remove_var(key_env) };
        assert_eq!(resolved, "eleven_flash_v2_5");
    }

    #[tokio::test]
    async fn resolver_sends_models_get_and_api_key_header() {
        let body = serde_json::json!([{"model_id": "eleven_multilingual_v2"}]);
        let (base_url, captured) =
            one_request_server(body.to_string().into_bytes(), "application/json");
        let key_env = "BOOKFORGE_ELEVENLABS_RESOLVER_REQUEST_TEST_KEY";
        unsafe { std::env::set_var(key_env, "resolver-request-key") };
        let resolved =
            resolve_preferred_elevenlabs_model(&resolver_config(base_url, key_env), 5_000, false)
                .await
                .unwrap();
        unsafe { std::env::remove_var(key_env) };

        assert_eq!(resolved, "eleven_multilingual_v2");
        let raw = captured.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(raw.starts_with("GET /v1/models HTTP/1.1"));
        assert!(
            raw.to_ascii_lowercase()
                .contains("xi-api-key: resolver-request-key")
        );
    }

    #[tokio::test]
    async fn resolver_errors_on_unparseable_body() {
        let (base_url, _) = one_request_server(b"not json".to_vec(), "application/json");
        let key_env = "BOOKFORGE_ELEVENLABS_RESOLVER_GARBAGE_TEST_KEY";
        unsafe { std::env::set_var(key_env, "resolver-garbage-key") };
        let error =
            resolve_preferred_elevenlabs_model(&resolver_config(base_url, key_env), 5_000, false)
                .await
                .unwrap_err();
        unsafe { std::env::remove_var(key_env) };
        assert!(matches!(error, TtsError::Provider(_)));
        assert!(error.to_string().contains("parse ElevenLabs models"));
    }

    #[tokio::test]
    async fn resolver_errors_on_empty_body() {
        let (base_url, _) = one_request_server(Vec::new(), "application/json");
        let key_env = "BOOKFORGE_ELEVENLABS_RESOLVER_EMPTY_TEST_KEY";
        unsafe { std::env::set_var(key_env, "resolver-empty-key") };
        let error =
            resolve_preferred_elevenlabs_model(&resolver_config(base_url, key_env), 5_000, false)
                .await
                .unwrap_err();
        unsafe { std::env::remove_var(key_env) };
        assert!(matches!(error, TtsError::Provider(_)));
        assert!(error.to_string().contains("empty response body"));
    }

    #[tokio::test]
    async fn subscription_get_parses_counts_and_sends_key() {
        let body = serde_json::json!({"character_count": 123, "character_limit": 456});
        let (base_url, captured) =
            one_request_server(body.to_string().into_bytes(), "application/json");
        let key_env = "BOOKFORGE_ELEVENLABS_SUBSCRIPTION_TEST_KEY";
        unsafe { std::env::set_var(key_env, "subscription-key") };
        let subscription = fetch_elevenlabs_subscription(&resolver_config(base_url, key_env))
            .await
            .unwrap();
        unsafe { std::env::remove_var(key_env) };
        assert_eq!(subscription.character_count, 123);
        assert_eq!(subscription.character_limit, 456);
        let raw = captured.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(raw.starts_with("GET /v1/user/subscription HTTP/1.1"));
        assert!(
            raw.to_ascii_lowercase()
                .contains("xi-api-key: subscription-key")
        );
    }

    #[tokio::test]
    async fn subscription_errors_on_garbage_body() {
        let (base_url, _) = one_request_server(b"garbage".to_vec(), "application/json");
        let key_env = "BOOKFORGE_ELEVENLABS_SUBSCRIPTION_GARBAGE_TEST_KEY";
        unsafe { std::env::set_var(key_env, "subscription-garbage-key") };
        let error = fetch_elevenlabs_subscription(&resolver_config(base_url, key_env))
            .await
            .unwrap_err();
        unsafe { std::env::remove_var(key_env) };
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
        let (base_url, captured) =
            one_request_server(body.to_string().into_bytes(), "application/json");
        let voices = list_elevenlabs_voices(&base_url, "voices-key", 5)
            .await
            .unwrap();
        assert_eq!(voices.len(), 1);
        assert_eq!(voices[0].voice_id, "voice-1");
        assert_eq!(voices[0].name, "Narrator");
        assert_eq!(voices[0].category, "premade");
        assert_eq!(voices[0].labels["accent"], "italian");
        let raw = captured.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(raw.starts_with("GET /v1/voices HTTP/1.1"));
        assert!(raw.to_ascii_lowercase().contains("xi-api-key: voices-key"));
    }

    #[tokio::test]
    async fn voices_errors_on_garbage_body() {
        let (base_url, _) = one_request_server(b"garbage".to_vec(), "application/json");
        let error = list_elevenlabs_voices(&base_url, "voices-garbage-key", 5)
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
        let (base_url, captured) = one_request_server(expected_audio.clone(), "audio/mpeg");
        let key_env = "BOOKFORGE_ELEVENLABS_TTS_CONTRACT_TEST_KEY";
        // SAFETY: this test uses a crate-specific variable that no production
        // code or parallel test reads.
        unsafe { std::env::set_var(key_env, "eleven-test-key") };
        let provider = ElevenLabsTtsProvider::new(ElevenLabsTtsConfig {
            base_url,
            api_key_env: key_env.to_string(),
            model: "eleven_multilingual_v2".to_string(),
            timeout_seconds: 5,
            max_attempts: 1,
        })
        .unwrap();
        let clip = provider
            .synthesize(request(AudioFormat::Mp3))
            .await
            .expect("mocked ElevenLabs synthesis");
        unsafe { std::env::remove_var(key_env) };

        assert_eq!(clip.bytes, expected_audio);
        let raw = captured.recv_timeout(Duration::from_secs(2)).unwrap();
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
