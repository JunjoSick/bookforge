use super::*;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/options", get(dashboard_options))
        .route("/api/providers", get(provider_status))
}

async fn dashboard_options() -> Json<DashboardOptions> {
    Json(dashboard_options_payload())
}

/// Report which providers already have a usable key — either remembered in this
/// session or present in the server's environment — so the UI only prompts when
/// a key is actually needed. Never returns key material.
async fn provider_status(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, AppError> {
    let remembered = lock_keys(&state)?;
    let mut status = serde_json::Map::new();
    for (provider, env) in PROVIDER_KEY_ENVS {
        let configured = remembered.contains_key(*provider)
            || std::env::var(env).map(|v| !v.is_empty()).unwrap_or(false);
        status.insert((*provider).to_string(), json!(configured));
    }
    for (provider, env) in AUDIO_PROVIDER_KEY_ENVS {
        let configured = remembered.contains_key(&format!("audio:{provider}"))
            || std::env::var(env)
                .map(|value| !value.is_empty())
                .unwrap_or(false);
        status.insert(format!("audio:{provider}"), json!(configured));
    }
    Ok(Json(serde_json::Value::Object(status)))
}

pub(super) fn dashboard_options_payload() -> DashboardOptions {
    DashboardOptions {
        languages: LANGUAGE_OPTIONS,
        providers: vec![
            ProviderOption {
                id: "mock",
                label: "mock (offline test)",
                models: MOCK_MODELS,
                default_model: "mock-identity",
                requires_base_url: false,
                requires_key: false,
            },
            ProviderOption {
                id: "deepseek",
                label: "deepseek",
                models: DEEPSEEK_MODELS,
                default_model: "deepseek-v4-flash",
                requires_base_url: false,
                requires_key: true,
            },
            ProviderOption {
                id: "openrouter",
                label: "openrouter",
                models: OPENROUTER_MODELS,
                default_model: "openrouter/auto",
                requires_base_url: false,
                requires_key: true,
            },
            ProviderOption {
                id: "openai-compatible",
                label: "openai-compatible",
                models: OPENAI_COMPATIBLE_MODELS,
                default_model: "gpt-4o-mini",
                requires_base_url: true,
                requires_key: true,
            },
        ],
        audio_providers: vec![
            AudioProviderOption {
                id: "mock",
                label: "mock (offline test)",
                models: AUDIO_MOCK_MODELS,
                default_model: "mock-silence",
                default_voice: "mock",
                formats: &["wav"],
                default_format: "wav",
                requires_voice: false,
                requires_key: false,
                supports_auto_model: false,
                supports_instructions: false,
                supports_speed: true,
                max_chars: 40_000,
                model_max_chars: BTreeMap::new(),
            },
            AudioProviderOption {
                id: "openai",
                label: "OpenAI-compatible",
                models: AUDIO_OPENAI_MODELS,
                default_model: "gpt-4o-mini-tts",
                default_voice: "alloy",
                formats: &["mp3", "opus", "aac", "flac", "wav", "pcm"],
                default_format: "mp3",
                requires_voice: false,
                requires_key: true,
                supports_auto_model: false,
                supports_instructions: true,
                supports_speed: true,
                max_chars: 4_096,
                model_max_chars: BTreeMap::new(),
            },
            AudioProviderOption {
                id: "gemini",
                label: "Gemini TTS",
                models: AUDIO_GEMINI_MODELS,
                default_model: "gemini-3.1-flash-tts-preview",
                default_voice: "Kore",
                formats: &["wav", "pcm"],
                default_format: "wav",
                requires_voice: false,
                requires_key: true,
                supports_auto_model: false,
                supports_instructions: true,
                supports_speed: false,
                max_chars: 4_096,
                model_max_chars: BTreeMap::new(),
            },
            AudioProviderOption {
                id: "elevenlabs",
                label: "ElevenLabs",
                models: AUDIO_ELEVENLABS_MODELS,
                default_model: "",
                default_voice: "",
                formats: &["mp3", "opus", "wav", "pcm"],
                default_format: "mp3",
                requires_voice: true,
                requires_key: true,
                supports_auto_model: true,
                supports_instructions: false,
                supports_speed: true,
                max_chars: bookforge_audio::elevenlabs_model_max_input_chars(
                    "eleven_multilingual_v2",
                ),
                model_max_chars: AUDIO_ELEVENLABS_MODELS
                    .iter()
                    .map(|model| {
                        (
                            *model,
                            bookforge_audio::elevenlabs_model_max_input_chars(model),
                        )
                    })
                    .collect(),
            },
        ],
        ffmpeg_available: bookforge_audio::ffmpeg_available(),
    }
}

#[derive(Serialize)]
pub(super) struct DashboardOptions {
    pub(super) languages: &'static [&'static str],
    pub(super) providers: Vec<ProviderOption>,
    pub(super) audio_providers: Vec<AudioProviderOption>,
    ffmpeg_available: bool,
}

#[derive(Serialize)]
pub(super) struct ProviderOption {
    pub(super) id: &'static str,
    label: &'static str,
    pub(super) models: &'static [&'static str],
    pub(super) default_model: &'static str,
    requires_base_url: bool,
    requires_key: bool,
}

#[derive(Serialize)]
pub(super) struct AudioProviderOption {
    pub(super) id: &'static str,
    label: &'static str,
    pub(super) models: &'static [&'static str],
    pub(super) default_model: &'static str,
    pub(super) default_voice: &'static str,
    pub(super) formats: &'static [&'static str],
    default_format: &'static str,
    pub(super) requires_voice: bool,
    requires_key: bool,
    pub(super) supports_auto_model: bool,
    pub(super) supports_instructions: bool,
    pub(super) supports_speed: bool,
    pub(super) max_chars: usize,
    /// Per-model input ceilings, so the browser never keeps its own copy of a
    /// provider's limits. Empty when every model shares `max_chars`.
    pub(super) model_max_chars: BTreeMap<&'static str, usize>,
}
