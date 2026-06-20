use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

const BUNDLED_PRICING: &str = include_str!("../pricing/providers.json");

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct TokenPrices {
    pub input_per_million: f64,
    pub output_per_million: f64,
    pub input_cache_per_million: Option<f64>,
}

#[derive(Debug, Clone)]
pub(crate) struct PricingCatalog {
    data: PricingFile,
    source: PricingSource,
}

#[derive(Debug, Clone)]
enum PricingSource {
    Bundled,
    File(PathBuf),
}

#[derive(Debug, Clone, Deserialize)]
struct PricingFile {
    schema_version: u32,
    #[allow(dead_code)]
    updated_at: String,
    providers: BTreeMap<String, ProviderPricing>,
}

#[derive(Debug, Clone, Deserialize)]
struct ProviderPricing {
    models: BTreeMap<String, ModelPricing>,
}

#[derive(Debug, Clone, Deserialize)]
struct ModelPricing {
    input_per_million_usd: f64,
    output_per_million_usd: f64,
    input_cache_per_million_usd: Option<f64>,
}

impl PricingCatalog {
    pub(crate) fn source_label(&self) -> String {
        match &self.source {
            PricingSource::Bundled => "bundled pricing/providers.json".to_string(),
            PricingSource::File(path) => path.display().to_string(),
        }
    }

    pub(crate) fn token_prices(&self, provider: &str, model: &str) -> Option<TokenPrices> {
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

pub(crate) fn load_pricing(explicit_path: Option<&Path>) -> Result<PricingCatalog> {
    let override_path = explicit_path
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("BOOKFORGE_PRICING_PATH").map(PathBuf::from));

    match override_path {
        Some(path) => {
            let content = fs::read_to_string(&path)
                .with_context(|| format!("reading pricing file {}", path.display()))?;
            parse_pricing(&content, PricingSource::File(path))
        }
        None => parse_pricing(BUNDLED_PRICING, PricingSource::Bundled),
    }
}

fn parse_pricing(content: &str, source: PricingSource) -> Result<PricingCatalog> {
    let data: PricingFile = serde_json::from_str(content).context("parsing pricing JSON")?;
    if data.schema_version != 1 {
        bail!(
            "unsupported pricing schema_version {}; expected 1",
            data.schema_version
        );
    }
    if data.providers.is_empty() {
        bail!("pricing catalog contains no providers");
    }
    Ok(PricingCatalog { data, source })
}

pub(crate) fn estimate_cost_usd_with_cached(
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

pub(crate) fn estimate_cost_usd_with_pricing(
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
    fn workspace_pricing_catalog_matches_packaged_copy_when_present() {
        let workspace_copy =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../pricing/providers.json");
        if workspace_copy.exists() {
            let workspace =
                fs::read_to_string(workspace_copy).expect("workspace pricing should read");
            assert_eq!(workspace.trim(), BUNDLED_PRICING.trim());
        }
    }

    #[test]
    fn explicit_pricing_file_overrides_bundled_catalog() {
        let temp = tempfile::tempdir().expect("temp dir should exist");
        let path = temp.path().join("pricing.json");
        fs::write(
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

        let catalog = load_pricing(Some(&path)).expect("custom pricing should parse");
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
    fn unsupported_schema_is_rejected() {
        let error = parse_pricing(
            r#"{"schema_version":2,"updated_at":"x","providers":{"x":{"models":{}}}}"#,
            PricingSource::Bundled,
        )
        .expect_err("schema 2 should be rejected");

        assert!(error.to_string().contains("schema_version 2"));
    }
}
