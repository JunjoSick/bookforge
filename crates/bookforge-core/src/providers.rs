//! Canonical home for static provider metadata: bundled pricing catalogs and
//! default endpoint tables.
//!
//! Every consumer (CLI, dashboard, judge tooling) loads pricing through the
//! typed schema below instead of re-implementing a JSON loader per site. The
//! physical catalogs live at `crates/bookforge-core/pricing/` because
//! [`include_str!`] must resolve inside the package directory for
//! builds-from-tarball (`cargo publish` cannot ship files outside it).
//!
//! Override semantics (preserved verbatim from the original CLI loaders):
//! - an explicit path wins over the environment variable,
//! - `BOOKFORGE_PRICING_PATH` overrides the bundled text-pricing catalog,
//! - `BOOKFORGE_AUDIO_PRICING_PATH` overrides the bundled audio catalog,
//! - parsing is fail-closed: unknown `schema_version`, empty provider sets,
//!   and invalid audio entries are hard errors.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use thiserror::Error;

/// Bundled text-provider pricing catalog (schema 1).
const BUNDLED_PRICING: &str = include_str!("../pricing/providers.json");
/// Bundled audio-provider pricing catalog (schema 1).
const BUNDLED_AUDIO_PRICING: &str = include_str!("../pricing/audio-providers.json");

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors surfaced while resolving or validating a pricing catalog.
///
/// Display strings are part of the CLI contract; they must match the messages
/// the pre-consolidation loaders printed.
#[derive(Debug, Error)]
pub enum PricingLoadError {
    #[error("reading pricing file {0}")]
    ReadPricing(PathBuf),
    #[error("parsing pricing JSON")]
    ParsePricing(#[source] serde_json::Error),
    #[error("unsupported pricing schema_version {0}; expected 1")]
    UnsupportedSchema(u32),
    #[error("pricing catalog contains no providers")]
    EmptyCatalog,

    #[error("reading audio pricing file {0}")]
    ReadAudioPricing(PathBuf),
    #[error("{context}")]
    ParseAudioPricing {
        context: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("unsupported audio pricing schema_version {0}; expected 1")]
    UnsupportedAudioSchema(u32),
    #[error("audio pricing catalog contains no providers")]
    EmptyAudioCatalog,
    #[error(
        "audio pricing entry {provider}/{model} must define usd_per_million_chars or credits_per_char"
    )]
    EmptyAudioEntry { provider: String, model: String },
    #[error("audio pricing entry {provider}/{model} has invalid {label}")]
    InvalidAudioValue {
        provider: String,
        model: String,
        label: String,
    },
}

// ---------------------------------------------------------------------------
// Text pricing (token-based)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TokenPrices {
    pub input_per_million: f64,
    pub output_per_million: f64,
    pub input_cache_per_million: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct PricingCatalog {
    data: PricingFile,
    source: PricingSource,
}

#[derive(Debug, Clone)]
enum PricingSource {
    Bundled,
    File(PathBuf),
}

#[derive(Debug, Clone, serde::Deserialize)]
struct PricingFile {
    schema_version: u32,
    #[allow(dead_code)]
    updated_at: String,
    providers: BTreeMap<String, ProviderPricing>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct ProviderPricing {
    models: BTreeMap<String, ModelPricing>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct ModelPricing {
    input_per_million_usd: f64,
    output_per_million_usd: f64,
    input_cache_per_million_usd: Option<f64>,
}

impl PricingCatalog {
    pub fn source_label(&self) -> String {
        match &self.source {
            PricingSource::Bundled => "bundled pricing/providers.json".to_string(),
            PricingSource::File(path) => path.display().to_string(),
        }
    }

    pub fn token_prices(&self, provider: &str, model: &str) -> Option<TokenPrices> {
        if provider.eq_ignore_ascii_case("mock") {
            return Some(TokenPrices {
                input_per_million: 0.0,
                output_per_million: 0.0,
                input_cache_per_million: Some(0.0),
            });
        }

        let provider = provider.to_ascii_lowercase();
        let model = model.to_ascii_lowercase();
        let pricing = self
            .data
            .providers
            .get(&provider)?
            .models
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(&model))
            .map(|(_, pricing)| pricing)?;
        Some(TokenPrices {
            input_per_million: pricing.input_per_million_usd,
            output_per_million: pricing.output_per_million_usd,
            input_cache_per_million: pricing.input_cache_per_million_usd,
        })
    }
}

/// Load the token-priced catalog: explicit path first, then
/// `BOOKFORGE_PRICING_PATH`, then the bundled default.
pub fn load_pricing(explicit_path: Option<&Path>) -> Result<PricingCatalog, PricingLoadError> {
    let override_path = explicit_path
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("BOOKFORGE_PRICING_PATH").map(PathBuf::from));

    match override_path {
        Some(path) => {
            let content = fs::read_to_string(&path)
                .map_err(|_| PricingLoadError::ReadPricing(path.clone()))?;
            parse_pricing(&content, PricingSource::File(path))
        }
        None => parse_pricing(BUNDLED_PRICING, PricingSource::Bundled),
    }
}

fn parse_pricing(content: &str, source: PricingSource) -> Result<PricingCatalog, PricingLoadError> {
    let data: PricingFile =
        serde_json::from_str(content).map_err(PricingLoadError::ParsePricing)?;
    if data.schema_version != 1 {
        return Err(PricingLoadError::UnsupportedSchema(data.schema_version));
    }
    if data.providers.is_empty() {
        return Err(PricingLoadError::EmptyCatalog);
    }
    Ok(PricingCatalog { data, source })
}

pub fn estimate_cost_usd_with_cached(
    provider: &str,
    model: &str,
    input_tokens: u64,
    input_cached_tokens: u64,
    output_tokens: u64,
) -> Option<f64> {
    let pricing = load_pricing(None).ok()?;
    estimate_cost_usd_with_pricing(
        &pricing,
        provider,
        model,
        input_tokens,
        input_cached_tokens,
        output_tokens,
    )
}

pub fn estimate_cost_usd_with_pricing(
    pricing: &PricingCatalog,
    provider: &str,
    model: &str,
    input_tokens: u64,
    input_cached_tokens: u64,
    output_tokens: u64,
) -> Option<f64> {
    let prices = pricing.token_prices(provider, model)?;
    let uncached_input = input_tokens.saturating_sub(input_cached_tokens);
    let cached_rate = prices
        .input_cache_per_million
        .unwrap_or(prices.input_per_million);
    Some(
        (uncached_input as f64 / 1_000_000.0 * prices.input_per_million)
            + (input_cached_tokens as f64 / 1_000_000.0 * cached_rate)
            + (output_tokens as f64 / 1_000_000.0 * prices.output_per_million),
    )
}

// ---------------------------------------------------------------------------
// Audio pricing (character-based)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioCost {
    pub usd: Option<f64>,
    pub credits: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct AudioPricingCatalog {
    data: AudioPricingFile,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct AudioPricingFile {
    schema_version: u32,
    #[allow(dead_code)]
    updated_at: String,
    providers: BTreeMap<String, BTreeMap<String, AudioModelPricing>>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct AudioModelPricing {
    usd_per_million_chars: Option<f64>,
    credits_per_char: Option<f64>,
    #[allow(dead_code)]
    note: String,
}

impl AudioPricingCatalog {
    pub fn estimate(&self, provider: &str, model: &str, chars: usize) -> Option<AudioCost> {
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

/// Load the character-priced audio catalog: explicit override path (the CLI
/// audiobook command passes its own), then `BOOKFORGE_AUDIO_PRICING_PATH`,
/// then the bundled default.
///
/// The original audio loader had no explicit-path parameter (it grew in the
/// dashboard first); callers that only need env-var semantics pass `None`.
pub fn load_audio_pricing() -> Result<AudioPricingCatalog, PricingLoadError> {
    match std::env::var_os("BOOKFORGE_AUDIO_PRICING_PATH").map(PathBuf::from) {
        Some(path) => {
            let content = fs::read_to_string(&path)
                .map_err(|_| PricingLoadError::ReadAudioPricing(path.clone()))?;
            parse_audio_pricing(&content, Some(&path))
        }
        None => parse_audio_pricing(BUNDLED_AUDIO_PRICING, None),
    }
}

pub fn estimate_audio_cost(provider: &str, model: &str, chars: usize) -> Option<AudioCost> {
    load_audio_pricing().ok()?.estimate(provider, model, chars)
}

fn parse_audio_pricing(
    content: &str,
    source: Option<&Path>,
) -> Result<AudioPricingCatalog, PricingLoadError> {
    let context = match source {
        Some(path) => format!("parsing audio pricing JSON from {}", path.display()),
        None => "parsing bundled audio pricing JSON".to_string(),
    };
    let data: AudioPricingFile =
        serde_json::from_str(content).map_err(|error| PricingLoadError::ParseAudioPricing {
            context,
            source: error,
        })?;
    if data.schema_version != 1 {
        return Err(PricingLoadError::UnsupportedAudioSchema(
            data.schema_version,
        ));
    }
    if data.providers.is_empty() {
        return Err(PricingLoadError::EmptyAudioCatalog);
    }
    for (provider, models) in &data.providers {
        for (model, pricing) in models {
            if pricing.usd_per_million_chars.is_none() && pricing.credits_per_char.is_none() {
                return Err(PricingLoadError::EmptyAudioEntry {
                    provider: provider.clone(),
                    model: model.clone(),
                });
            }
            for (label, value) in [
                ("usd_per_million_chars", pricing.usd_per_million_chars),
                ("credits_per_char", pricing.credits_per_char),
            ] {
                if value.is_some_and(|value| !value.is_finite() || value < 0.0) {
                    return Err(PricingLoadError::InvalidAudioValue {
                        provider: provider.clone(),
                        model: model.clone(),
                        label: label.to_string(),
                    });
                }
            }
        }
    }
    Ok(AudioPricingCatalog { data })
}

// ---------------------------------------------------------------------------
// Default endpoint registry
// ---------------------------------------------------------------------------

/// Static default endpoint metadata for one named provider.
///
/// `default_model` is `None` when callers must select a model explicitly
/// (`openai-compatible`): doctor-style tools substitute
/// [`LOCAL_MODEL_PLACEHOLDER`], while strict translation runs reject the run.
#[derive(Debug, Clone, Copy)]
pub struct ProviderDefaults {
    pub id: &'static str,
    /// Canonical API base URL; `None` when the caller must supply one.
    pub base_url: Option<&'static str>,
    /// Name (not value) of the environment variable holding the API key.
    pub api_key_env: &'static str,
    pub default_model: Option<&'static str>,
}

pub const UNKNOWN_MODEL_PLACEHOLDER: &str = "unknown";
pub const LOCAL_MODEL_PLACEHOLDER: &str = "local-model";
pub const MOCK_PROVIDER_ID: &str = "mock";
pub const MOCK_DEFAULT_MODEL: &str = "mock-prefix-target";

/// Registry consulted by translate defaults, plan inspection, estimate
/// fallbacks, doctor checks, and judge tooling. Lookup is exact-match on the
/// lowercase provider ids, mirroring the literal tables it replaces.
pub const PROVIDER_ENDPOINT_DEFAULTS: &[ProviderDefaults] = &[
    ProviderDefaults {
        id: "deepseek",
        base_url: Some("https://api.deepseek.com/v1"),
        api_key_env: "DEEPSEEK_API_KEY",
        default_model: Some("deepseek-v4-flash"),
    },
    ProviderDefaults {
        id: "openrouter",
        base_url: Some("https://openrouter.ai/api/v1"),
        api_key_env: "OPENROUTER_API_KEY",
        default_model: Some("openrouter/auto"),
    },
    ProviderDefaults {
        id: "openai-compatible",
        base_url: None,
        api_key_env: "OPENAI_API_KEY",
        default_model: None,
    },
    ProviderDefaults {
        id: "local-ollama",
        base_url: Some("http://localhost:11434/v1"),
        api_key_env: "OLLAMA_API_KEY",
        default_model: Some("qwen2.5:14b"),
    },
    ProviderDefaults {
        id: "local-llamacpp",
        base_url: Some("http://localhost:8080/v1"),
        api_key_env: "LLAMACPP_API_KEY",
        default_model: Some("local-model"),
    },
];

pub fn provider_defaults(id: &str) -> Option<&'static ProviderDefaults> {
    PROVIDER_ENDPOINT_DEFAULTS
        .iter()
        .find(|entry| entry.id == id)
}

/// Default model id shown when no explicit model was selected.
///
/// `mock` maps to its sentinel translation mode; unknown providers yield the
/// historic `"unknown"` placeholder rather than an error.
pub fn default_model_id(provider: &str) -> &'static str {
    match provider {
        MOCK_PROVIDER_ID => MOCK_DEFAULT_MODEL,
        other => provider_defaults(other)
            .and_then(|defaults| defaults.default_model)
            .unwrap_or(UNKNOWN_MODEL_PLACEHOLDER),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_catalog_contains_roadmap_openrouter_model() {
        let catalog = load_pricing(None).expect("bundled pricing should parse");
        let prices = catalog
            .token_prices("openrouter", "deepseek/deepseek-v4-flash")
            .expect("roadmap model should be priced");

        assert_eq!(prices.input_per_million, 0.14);
        assert_eq!(prices.output_per_million, 0.28);
        assert_eq!(prices.input_cache_per_million, Some(0.0028));
    }

    #[test]
    fn bundled_catalog_source_label_matches_pre_consolidation_wording() {
        let catalog = load_pricing(None).expect("bundled pricing should parse");
        assert_eq!(catalog.source_label(), "bundled pricing/providers.json");
    }

    #[test]
    fn explicit_pricing_file_overrides_bundled_catalog() {
        let temp = std::env::temp_dir().join(format!("bf-pricing-override-{}", std::process::id()));
        std::fs::create_dir_all(&temp).expect("temp dir should create");
        let path = temp.join("pricing.json");
        std::fs::write(
            &path,
            r#"{
  "schema_version": 1,
  "updated_at": "2026-06-20",
  "providers": {
    "deepseek": {
      "models": {
        "test-model": {
          "input_per_million_usd": 1.0,
          "output_per_million_usd": 2.0,
          "input_cache_per_million_usd": 0.5
        }
      }
    }
  }
}"#,
        )
        .expect("custom pricing should write");

        let result = load_pricing(Some(&path));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&temp);

        let catalog = result.expect("custom pricing should parse");
        let cost = estimate_cost_usd_with_pricing(
            &catalog,
            "deepseek",
            "test-model",
            1_000_000,
            0,
            1_000_000,
        )
        .expect("custom model should be priced");

        assert_eq!(cost, 3.0);
        assert_eq!(catalog.source_label(), path.display().to_string());
    }

    #[test]
    fn billable_output_total_including_reasoning_is_charged_once() {
        let catalog = parse_pricing(
            r#"{
  "schema_version": 1,
  "updated_at": "2026-07-30",
  "providers": {
    "openrouter": {
      "models": {
        "reasoning-model": {
          "input_per_million_usd": 0.0,
          "output_per_million_usd": 15.0,
          "input_cache_per_million_usd": null
        }
      }
    }
  }
}"#,
            PricingSource::Bundled,
        )
        .expect("test pricing should parse");

        // The provider layer folds any reasoning usage into this billable
        // output aggregate. The cost layer must price that aggregate once.
        let cost = estimate_cost_usd_with_pricing(
            &catalog,
            "openrouter",
            "reasoning-model",
            0,
            0,
            182_000,
        )
        .expect("test model should be priced");

        assert!((cost - 2.73).abs() < f64::EPSILON);
    }

    #[test]
    fn unsupported_schema_is_rejected() {
        let error = parse_pricing(
            r#"{"schema_version":2,"updated_at":"x","providers":{"x":{"models":{}}}}"#,
            PricingSource::Bundled,
        )
        .expect_err("schema 2 should be rejected");

        assert!(error.to_string().contains("schema_version 2"));
    }

    #[test]
    fn empty_catalog_is_rejected() {
        let error = parse_pricing(
            r#"{"schema_version":1,"updated_at":"x","providers":{}}"#,
            PricingSource::Bundled,
        )
        .expect_err("empty catalog should be rejected");
        assert!(matches!(error, PricingLoadError::EmptyCatalog));
    }

    const TEST_AUDIO_PRICING: &str = r#"{
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
        let pricing = parse_audio_pricing(TEST_AUDIO_PRICING, None).expect("pricing should parse");
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
        let pricing = parse_audio_pricing(TEST_AUDIO_PRICING, None).expect("pricing should parse");
        assert_eq!(pricing.estimate("test", "missing", 10), None);
        assert_eq!(pricing.estimate("missing", "voice-model", 10), None);
    }

    #[test]
    fn bundled_audio_pricing_parses_and_public_lookup_hits() {
        let pricing =
            parse_audio_pricing(BUNDLED_AUDIO_PRICING, None).expect("bundled pricing should parse");
        assert!(pricing.estimate("elevenlabs", "eleven_v3", 100).is_some());
        assert!(estimate_audio_cost("openai", "tts-1", 100).is_some());
    }

    #[test]
    fn bundled_audio_catalog_entries_validate_their_rates() {
        let error = parse_audio_pricing(
            r#"{"schema_version":1,"updated_at":"x","providers":{"t":{"m":{"note":"none"}}}}"#,
            None,
        )
        .expect_err("entries without rates are rejected");
        assert!(
            error
                .to_string()
                .contains("must define usd_per_million_chars or credits_per_char")
        );
    }

    #[test]
    fn endpoint_registry_matches_historic_literal_tables() {
        let deepseek = provider_defaults("deepseek").expect("deepseek is registered");
        assert_eq!(deepseek.base_url, Some("https://api.deepseek.com/v1"));
        assert_eq!(deepseek.api_key_env, "DEEPSEEK_API_KEY");
        assert_eq!(deepseek.default_model, Some("deepseek-v4-flash"));

        let openrouter = provider_defaults("openrouter").expect("openrouter is registered");
        assert_eq!(openrouter.base_url, Some("https://openrouter.ai/api/v1"));
        assert_eq!(openrouter.default_model, Some("openrouter/auto"));

        let compatible =
            provider_defaults("openai-compatible").expect("openai-compatible is registered");
        assert!(compatible.base_url.is_none());
        assert!(compatible.default_model.is_none());

        let ollama = provider_defaults("local-ollama").expect("local-ollama is registered");
        assert_eq!(ollama.default_model, Some("qwen2.5:14b"));

        let llamacpp = provider_defaults("local-llamacpp").expect("local-llamacpp is registered");
        assert_eq!(llamacpp.default_model, Some("local-model"));
    }

    #[test]
    fn default_model_id_preserves_placeholder_semantics() {
        assert_eq!(default_model_id("mock"), "mock-prefix-target");
        assert_eq!(default_model_id("deepseek"), "deepseek-v4-flash");
        assert_eq!(default_model_id("openrouter"), "openrouter/auto");
        // openai-compatible historically resolved to "unknown" in estimate/plan
        // contexts; doctor/judge tooling substitutes LOCAL_MODEL_PLACEHOLDER.
        assert_eq!(default_model_id("openai-compatible"), "unknown");
        assert_eq!(default_model_id("anything-else"), "unknown");
    }

    /// Packaging guard: exactly one tree copy of each catalog exists — the one
    /// embedded by this module. Re-introducing copies elsewhere breaks the
    /// single-catalog invariant auditors keep flagging (DUP §5).
    #[test]
    fn pricing_catalogs_have_no_duplicate_tree_copies() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let workspace_root = manifest_dir
            .parent()
            .and_then(Path::parent)
            .expect("core crate lives inside the workspace");
        let legacy_locations = [
            workspace_root.join("pricing/providers.json"),
            workspace_root.join("crates/bookforge-cli/pricing/providers.json"),
            workspace_root.join("crates/bookforge-cli/pricing/audio-providers.json"),
        ];
        for legacy in legacy_locations {
            assert!(
                !legacy.exists(),
                "stale duplicate pricing copy found at {}; remove it — \
                 crates/bookforge-core/pricing/ is the only catalog location",
                legacy.display()
            );
        }
    }
}
