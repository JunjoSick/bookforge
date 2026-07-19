use tokio_util::sync::CancellationToken;

use super::{
    AudioClip, AudioFormat, Result, SpeechRequest, TtsError, TtsProvider, build_http_client,
    required_api_key, send_with_retry, validate_audio_payload, validate_path_component,
};

/// Absolute maximum Unicode characters accepted by an ElevenLabs TTS model.
pub const ELEVENLABS_MAX_INPUT_CHARS: usize = 40_000;

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
    serde_json::json!({
        "text": request.text,
        "model_id": model,
        "voice_settings": {"speed": request.speed}
    })
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
    }
}
