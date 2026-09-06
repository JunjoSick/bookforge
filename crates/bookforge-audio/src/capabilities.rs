//! Per-provider accepted-option matrix (AUDIO-6 / AUDIO-8 / ASYM-1).
//!
//! The dashboard historically validated provider knobs far away from the
//! providers themselves: a `seed` sent to OpenAI or Gemini was accepted at
//! launch and only failed (or silently did nothing) inside the child, and
//! `--text-normalization` was swallowed without a trace for OpenAI/Gemini in
//! the CLI option construction. This module is the single source of truth
//! for what each TTS backend actually reads from a
//! [`SpeechRequest`](crate::provider::SpeechRequest), as pure functions so
//! both CLI and serve can reject or warn-and-drop an option *before* spend.
//!
//! Truth table is derived from what each request body builder emits:
//! - ElevenLabs native body sends seed, language_code,
//!   apply_text_normalization, and voice_settings.speed.
//! - OpenAI-compatible `/audio/speech` sends instructions and speed; it has
//!   no fields for the other three.
//! - Gemini generationConfig carries only the voice; playback speed is
//!   rejected by the provider itself, and no field exists for seed,
//!   language, or text normalization.
//! - The mock provider accepts everything by design and ignores all knobs.

use crate::provider::TtsProviderKind;

/// Every launch-shaping knob a caller can request, and whether the given
/// provider will actually use it. Options marked unsupported must be
/// warn-and-dropped (or rejected) at option construction — never silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderFeatureSet {
    pub seed: bool,
    pub language_code: bool,
    pub text_normalization: bool,
    pub instructions: bool,
    pub speed: bool,
}

impl ProviderFeatureSet {
    /// Human-facing names used verbatim in warnings, keyed off the feature
    /// flags so messages cannot drift from behavior.
    pub fn unsupported_names(&self) -> Vec<&'static str> {
        let mut names = Vec::new();
        if !self.seed {
            names.push("--seed");
        }
        if !self.language_code {
            names.push("--language");
        }
        if !self.text_normalization {
            names.push("--text-normalization");
        }
        if !self.instructions {
            names.push("--instructions");
        }
        if !self.speed {
            names.push("--speed");
        }
        names
    }
}

/// The exact option matrix for `provider`. Unknown ids yield `None` so
/// callers distinguish "unsupported value" from "provider not recognized".
///
/// NOTE on speed nuance: ElevenLabs supports speed on every preferred model
/// *except* `eleven_v3`; that model-specific rejection stays in the request
/// path (`TtsError::Provider`) because auto-model resolution chooses v3 only
/// when speed control is explicitly not needed.
pub fn feature_set(provider: TtsProviderKind) -> ProviderFeatureSet {
    match provider {
        TtsProviderKind::Mock => ProviderFeatureSet {
            seed: false,
            language_code: false,
            text_normalization: false,
            instructions: false,
            speed: false,
        },
        TtsProviderKind::OpenAi => ProviderFeatureSet {
            seed: false,
            language_code: false,
            text_normalization: false,
            instructions: true,
            speed: true,
        },
        TtsProviderKind::Gemini => ProviderFeatureSet {
            seed: false,
            language_code: false,
            text_normalization: false,
            instructions: true,
            speed: false,
        },
        TtsProviderKind::ElevenLabs => ProviderFeatureSet {
            seed: true,
            language_code: true,
            text_normalization: true,
            instructions: false,
            speed: true,
        },
    }
}

/// Convenience wrapper mirroring how dashboards receive the provider id.
pub fn feature_set_for_id(provider_id: &str) -> Option<ProviderFeatureSet> {
    TtsProviderKind::parse(provider_id).map(feature_set)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elevenlabs_reads_every_knob_except_freeform_instructions() {
        let features = feature_set(TtsProviderKind::ElevenLabs);
        assert!(features.seed);
        assert!(features.language_code);
        assert!(features.text_normalization);
        assert!(features.speed);
        assert!(!features.instructions);
    }

    #[test]
    fn openai_and_gemini_ignore_seed_language_and_text_normalization() {
        for provider in [TtsProviderKind::OpenAi, TtsProviderKind::Gemini] {
            let features = feature_set(provider);
            assert!(!features.seed, "{provider:?}");
            assert!(!features.language_code, "{provider:?}");
            assert!(!features.text_normalization, "{provider:?}");
        }
    }

    #[test]
    fn gemini_takes_instructions_but_not_speed() {
        let features = feature_set(TtsProviderKind::Gemini);
        assert!(features.instructions);
        assert!(!features.speed);
    }

    #[test]
    fn warning_names_cover_exactly_the_unsupported_flags() {
        let gemini = feature_set(TtsProviderKind::Gemini).unsupported_names();
        assert_eq!(
            gemini,
            vec!["--seed", "--language", "--text-normalization", "--speed"]
        );
    }

    #[test]
    fn id_lookup_round_trips_and_rejects_unknown_providers() {
        assert_eq!(
            feature_set_for_id("elevenlabs"),
            Some(feature_set(TtsProviderKind::ElevenLabs))
        );
        assert_eq!(feature_set_for_id("mock").map(|f| f.seed), Some(false));
        assert_eq!(feature_set_for_id("anthropic"), None);
    }
}
