use anyhow::{Result, bail};

#[derive(Debug, Clone)]
pub struct Endpoint {
    pub provider: String,
    pub base_url: String,
    pub api_key_env: String,
    pub model: String,
}

pub fn resolve_endpoint(
    provider: &str,
    base_url: &Option<String>,
    api_key_env: &Option<String>,
    model: &Option<String>,
) -> Result<Endpoint> {
    let defaults = match provider {
        "deepseek" | "openrouter" | "openai-compatible" => {
            bookforge_core::providers::provider_defaults(provider)
                .expect("allow-list above matches registry entries")
        }
        other => {
            bail!("unsupported provider '{other}'; use deepseek, openrouter, or openai-compatible")
        }
    };
    if defaults.base_url.is_none() && base_url.is_none() {
        bail!("--provider openai-compatible requires --base-url");
    }

    Ok(Endpoint {
        provider: provider.to_string(),
        base_url: base_url
            .clone()
            .unwrap_or_else(|| defaults.base_url.unwrap_or_default().to_string()),
        api_key_env: api_key_env
            .clone()
            .unwrap_or_else(|| defaults.api_key_env.to_string()),
        model: model.clone().unwrap_or_else(|| {
            defaults
                .default_model
                .unwrap_or(bookforge_core::providers::LOCAL_MODEL_PLACEHOLDER)
                .to_string()
        }),
    })
}

pub fn strip_json_code_fence(body: &str) -> &str {
    let Some(inner) = body.strip_prefix("```") else {
        return body;
    };
    let Some(inner) = inner.strip_suffix("```") else {
        return body;
    };
    let Some((tag, payload)) = inner.split_once('\n') else {
        return body;
    };
    let tag = tag.trim();
    if tag.is_empty() || tag.eq_ignore_ascii_case("json") {
        payload.trim()
    } else {
        body
    }
}

#[allow(dead_code)] // Not every judge needs excerpt truncation.
pub fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    text.chars().take(max_chars).collect()
}
