use base64::Engine as _;
use tokio_util::sync::CancellationToken;

use super::{
    AudioClip, AudioFormat, MAX_AUDIO_RESPONSE_BODY_BYTES, Result, SpeechRequest, TtsError,
    TtsProvider, build_http_client, pcm_s16le_mono_wav, required_api_key, send_with_retry,
    validate_path_component,
};

/// Configuration for Google's Gemini Generate Content TTS endpoint.
#[derive(Debug, Clone)]
pub struct GeminiTtsConfig {
    pub base_url: String,
    pub api_key_env: String,
    pub model: String,
    pub timeout_seconds: u64,
    pub max_attempts: usize,
}

impl GeminiTtsConfig {
    pub fn google(model: Option<String>) -> Self {
        Self {
            base_url: "https://generativelanguage.googleapis.com/v1beta".to_string(),
            api_key_env: "GEMINI_API_KEY".to_string(),
            model: model.unwrap_or_else(|| "gemini-3.1-flash-tts-preview".to_string()),
            timeout_seconds: 120,
            max_attempts: 4,
        }
    }
}

#[derive(Debug, Clone)]
pub struct GeminiTtsProvider {
    config: GeminiTtsConfig,
    client: reqwest::Client,
    cancel_token: CancellationToken,
}

impl GeminiTtsProvider {
    pub fn new(config: GeminiTtsConfig) -> Result<Self> {
        Self::new_with_cancel(config, CancellationToken::new())
    }

    pub fn new_with_cancel(
        config: GeminiTtsConfig,
        cancel_token: CancellationToken,
    ) -> Result<Self> {
        validate_path_component(&config.model, "Gemini model")?;
        let client = build_http_client(config.timeout_seconds)?;
        Ok(Self {
            config,
            client,
            cancel_token,
        })
    }

    fn endpoint(&self) -> String {
        format!(
            "{}/models/{}:generateContent",
            self.config.base_url.trim_end_matches('/'),
            self.config.model
        )
    }
}

impl TtsProvider for GeminiTtsProvider {
    async fn synthesize(&self, request: SpeechRequest) -> Result<AudioClip> {
        if !matches!(request.format, AudioFormat::Wav | AudioFormat::Pcm) {
            return Err(TtsError::UnsupportedFormat(request.format.extension()));
        }
        if (request.speed - 1.0).abs() > f32::EPSILON {
            return Err(TtsError::Provider(
                "Gemini TTS does not expose a playback-speed control; use --speed 1.0".to_string(),
            ));
        }
        let api_key = required_api_key(&self.config.api_key_env)?;
        let endpoint = self.endpoint();
        let body = gemini_request_body(&self.config.model, &request);
        // Gemini returns base64-encoded audio inside JSON, so this response
        // needs the audio ceiling rather than the metadata ceiling.
        let response = send_with_retry(
            &self.cancel_token,
            self.config.max_attempts,
            MAX_AUDIO_RESPONSE_BODY_BYTES,
            || {
                self.client
                    .post(&endpoint)
                    .header("x-goog-api-key", &api_key)
                    .json(&body)
            },
        )
        .await?;
        if response
            .content_type
            .as_deref()
            .is_some_and(|content_type| !content_type.to_ascii_lowercase().contains("json"))
        {
            return Err(TtsError::Provider(
                "Gemini returned an unexpected non-JSON response".to_string(),
            ));
        }
        let pcm = decode_gemini_audio(&response.bytes)?;
        if !pcm.len().is_multiple_of(2) {
            return Err(TtsError::Provider(
                "Gemini returned an odd-length PCM payload".to_string(),
            ));
        }
        let bytes = if request.format == AudioFormat::Wav {
            pcm_s16le_mono_wav(24_000, &pcm)
        } else {
            pcm
        };
        Ok(AudioClip {
            bytes,
            format: request.format,
        })
    }
}

fn gemini_request_body(model: &str, request: &SpeechRequest) -> serde_json::Value {
    let prompt = match request
        .instructions
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        Some(instructions) => format!(
            "## Director's notes\n{instructions}\n\n## Transcript\n{}",
            request.text
        ),
        None => request.text.clone(),
    };
    serde_json::json!({
        "contents": [{"parts": [{"text": prompt}]}],
        "generationConfig": {
            "responseModalities": ["AUDIO"],
            "speechConfig": {
                "voiceConfig": {
                    "prebuiltVoiceConfig": {"voiceName": request.voice}
                }
            }
        },
        "model": model
    })
}

fn decode_gemini_audio(response: &[u8]) -> Result<Vec<u8>> {
    let value: serde_json::Value = serde_json::from_slice(response)
        .map_err(|error| TtsError::Provider(format!("Gemini returned invalid JSON: {error}")))?;
    let parts = value
        .get("candidates")
        .and_then(serde_json::Value::as_array)
        .and_then(|candidates| candidates.first())
        .and_then(|candidate| candidate.pointer("/content/parts"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| TtsError::Provider("Gemini response has no audio parts".to_string()))?;
    let encoded = parts
        .iter()
        .find_map(|part| {
            part.pointer("/inlineData/data")
                .and_then(serde_json::Value::as_str)
        })
        .ok_or_else(|| {
            TtsError::Provider("Gemini response has no inline audio data".to_string())
        })?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| {
            TtsError::Provider(format!("Gemini returned invalid base64 audio: {error}"))
        })?;
    if bytes.is_empty() {
        return Err(TtsError::Provider(
            "Gemini returned empty inline audio data".to_string(),
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::test_support::{
        CAPTURE_WINDOW, one_request_server, retry_transient_transport,
    };

    fn request(format: AudioFormat) -> SpeechRequest {
        SpeechRequest {
            text: "toki pona".to_string(),
            voice: "Kore".to_string(),
            format,
            speed: 1.0,
            instructions: Some("Speak clearly.".to_string()),
            ..SpeechRequest::default()
        }
    }

    #[test]
    fn body_uses_native_gemini_audio_contract() {
        let body = gemini_request_body("gemini-3.1-flash-tts-preview", &request(AudioFormat::Wav));
        assert_eq!(body["generationConfig"]["responseModalities"][0], "AUDIO");
        assert_eq!(
            body["generationConfig"]["speechConfig"]["voiceConfig"]["prebuiltVoiceConfig"]["voiceName"],
            "Kore"
        );
        assert!(
            body["contents"][0]["parts"][0]["text"]
                .as_str()
                .unwrap()
                .contains("## Transcript")
        );
    }

    #[test]
    fn response_audio_is_decoded_and_can_be_wrapped_as_wav() {
        let encoded = base64::engine::general_purpose::STANDARD.encode([1u8, 2, 3, 4]);
        let response = serde_json::json!({
            "candidates": [{"content": {"parts": [{"inlineData": {
                "mimeType": "audio/L16;codec=pcm;rate=24000",
                "data": encoded
            }}]}}]
        });
        let pcm = decode_gemini_audio(response.to_string().as_bytes()).unwrap();
        assert_eq!(pcm, vec![1, 2, 3, 4]);
        let wav = pcm_s16le_mono_wav(24_000, &pcm);
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[44..], pcm.as_slice());
    }

    #[tokio::test]
    async fn rejects_formats_gemini_tts_does_not_return() {
        let provider = GeminiTtsProvider::new(GeminiTtsConfig::google(None)).unwrap();
        let error = provider
            .synthesize(request(AudioFormat::Mp3))
            .await
            .unwrap_err();
        assert!(matches!(error, TtsError::UnsupportedFormat("mp3")));
    }

    #[tokio::test]
    async fn sends_gemini_header_path_and_json_to_mock_server() {
        let encoded = base64::engine::general_purpose::STANDARD.encode([1u8, 2, 3, 4]);
        let response = serde_json::json!({
            "candidates": [{"content": {"parts": [{"inlineData": {
                "mimeType": "audio/L16;codec=pcm;rate=24000",
                "data": encoded
            }}]}}]
        })
        .to_string()
        .into_bytes();
        let key_env = "BOOKFORGE_GEMINI_TTS_CONTRACT_TEST_KEY";
        let (clip_bytes, raw) = retry_transient_transport(|| {
            let response = response.clone();
            async move {
                let (base_url, captured) = one_request_server(response, "application/json");
                // SAFETY: this test uses a crate-specific variable that no
                // production code or parallel test reads.
                unsafe { std::env::set_var(key_env, "gemini-test-key") };
                let provider = GeminiTtsProvider::new(GeminiTtsConfig {
                    base_url,
                    api_key_env: key_env.to_string(),
                    model: "gemini-3.1-flash-tts-preview".to_string(),
                    timeout_seconds: 5,
                    max_attempts: 1,
                })
                .unwrap();
                let clip = provider.synthesize(request(AudioFormat::Wav)).await;
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
        .expect("mocked Gemini synthesis");

        assert_eq!(&clip_bytes[..4], b"RIFF");
        let lowercase = raw.to_ascii_lowercase();
        assert!(
            raw.starts_with(
                "POST /v1/models/gemini-3.1-flash-tts-preview:generateContent HTTP/1.1"
            )
        );
        assert!(lowercase.contains("x-goog-api-key: gemini-test-key"));
        let body: serde_json::Value =
            serde_json::from_str(raw.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["generationConfig"]["responseModalities"][0], "AUDIO");
    }
}
