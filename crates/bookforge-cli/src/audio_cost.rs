use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

const BUNDLED_PRICING: &str = include_str!("../pricing/audio-providers.json");

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct AudioCost {
    pub usd: Option<f64>,
    pub credits: Option<f64>,
}

#[derive(Debug, Clone)]
pub(crate) struct AudioPricingCatalog {
    data: AudioPricingFile,
}

#[derive(Debug, Clone, Deserialize)]
struct AudioPricingFile {
    schema_version: u32,
    #[allow(dead_code)]
    updated_at: String,
    providers: BTreeMap<String, BTreeMap<String, AudioModelPricing>>,
}

#[derive(Debug, Clone, Deserialize)]
struct AudioModelPricing {
    usd_per_million_chars: Option<f64>,
    credits_per_char: Option<f64>,
    #[allow(dead_code)]
    note: String,
}

impl AudioPricingCatalog {
    pub(crate) fn estimate(&self, provider: &str, model: &str, chars: usize) -> Option<AudioCost> {
        let provider = self
            .data
            .providers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(provider))?
            .1;
        let pricing = provider
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(model))?
            .1;
        Some(AudioCost {
            usd: pricing
                .usd_per_million_chars
                .map(|rate| chars as f64 / 1_000_000.0 * rate),
            credits: pricing.credits_per_char.map(|rate| chars as f64 * rate),
        })
    }
}

pub(crate) fn load_audio_pricing() -> Result<AudioPricingCatalog> {
    let override_path = std::env::var_os("BOOKFORGE_AUDIO_PRICING_PATH").map(PathBuf::from);
    match override_path {
        Some(path) => {
            let content = fs::read_to_string(&path)
                .with_context(|| format!("reading audio pricing file {}", path.display()))?;
            parse_audio_pricing(&content, Some(&path))
        }
        None => parse_audio_pricing(BUNDLED_PRICING, None),
    }
}

pub(crate) fn estimate_audio_cost(provider: &str, model: &str, chars: usize) -> Option<AudioCost> {
    load_audio_pricing().ok()?.estimate(provider, model, chars)
}

fn parse_audio_pricing(content: &str, source: Option<&Path>) -> Result<AudioPricingCatalog> {
    let data: AudioPricingFile = serde_json::from_str(content).with_context(|| match source {
        Some(path) => format!("parsing audio pricing JSON from {}", path.display()),
        None => "parsing bundled audio pricing JSON".to_string(),
    })?;
    if data.schema_version != 1 {
        bail!(
            "unsupported audio pricing schema_version {}; expected 1",
            data.schema_version
        );
    }
    if data.providers.is_empty() {
        bail!("audio pricing catalog contains no providers");
    }
    for (provider, models) in &data.providers {
        for (model, pricing) in models {
            if pricing.usd_per_million_chars.is_none() && pricing.credits_per_char.is_none() {
                bail!(
                    "audio pricing entry {provider}/{model} must define usd_per_million_chars or credits_per_char"
                );
            }
            for (label, value) in [
                ("usd_per_million_chars", pricing.usd_per_million_chars),
                ("credits_per_char", pricing.credits_per_char),
            ] {
                if value.is_some_and(|value| !value.is_finite() || value < 0.0) {
                    bail!("audio pricing entry {provider}/{model} has invalid {label}");
                }
            }
        }
    }
    Ok(AudioPricingCatalog { data })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_PRICING: &str = r#"{
      "schema_version": 1,
      "updated_at": "2026-07-20",
      "providers": {
        "test": {
          "voice-model": {
            "usd_per_million_chars": 20.0,
            "credits_per_char": 0.25,
            "note": "test estimate"
          }
        }
      }
    }"#;

    #[test]
    fn parses_and_estimates_audio_pricing() {
        let pricing = parse_audio_pricing(TEST_PRICING, None).expect("pricing should parse");
        let cost = pricing
            .estimate("TEST", "VOICE-MODEL", 2_000_000)
            .expect("model should be found case-insensitively");
        assert_eq!(cost.usd, Some(40.0));
        assert_eq!(cost.credits, Some(500_000.0));
    }

    #[test]
    fn rejects_wrong_audio_pricing_schema() {
        let error = parse_audio_pricing(
            r#"{"schema_version":2,"updated_at":"x","providers":{"x":{}}}"#,
            None,
        )
        .expect_err("schema 2 should be rejected");
        assert!(error.to_string().contains("schema_version 2"));
    }

    #[test]
    fn audio_pricing_lookup_misses_unknown_entries() {
        let pricing = parse_audio_pricing(TEST_PRICING, None).expect("pricing should parse");
        assert_eq!(pricing.estimate("test", "missing", 10), None);
        assert_eq!(pricing.estimate("missing", "voice-model", 10), None);
    }

    #[test]
    fn bundled_audio_pricing_parses_and_public_lookup_hits() {
        let pricing =
            parse_audio_pricing(BUNDLED_PRICING, None).expect("bundled pricing should parse");
        assert!(pricing.estimate("elevenlabs", "eleven_v3", 100).is_some());
        assert!(estimate_audio_cost("openai", "tts-1", 100).is_some());
    }
}
