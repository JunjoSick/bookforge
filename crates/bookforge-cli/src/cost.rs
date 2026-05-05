#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct TokenPrices {
    pub input_per_million: f64,
    pub output_per_million: f64,
}

pub(crate) fn estimate_cost_usd(
    provider: &str,
    model: &str,
    input_tokens: u64,
    output_tokens: u64,
) -> Option<f64> {
    let prices = token_prices(provider, model)?;
    Some(
        (input_tokens as f64 / 1_000_000.0 * prices.input_per_million)
            + (output_tokens as f64 / 1_000_000.0 * prices.output_per_million),
    )
}

pub(crate) fn token_prices(provider: &str, model: &str) -> Option<TokenPrices> {
    let provider = provider.to_ascii_lowercase();
    let model = model.to_ascii_lowercase();

    if provider == "mock" {
        return Some(TokenPrices {
            input_per_million: 0.0,
            output_per_million: 0.0,
        });
    }

    if provider == "deepseek" {
        return match model.as_str() {
            "deepseek-v4-flash" | "deepseek-chat" => Some(TokenPrices {
                input_per_million: 0.27,
                output_per_million: 1.10,
            }),
            "deepseek-reasoner" => Some(TokenPrices {
                input_per_million: 0.55,
                output_per_million: 2.19,
            }),
            _ => None,
        };
    }

    match model.as_str() {
        "gpt-4o-mini" => Some(TokenPrices {
            input_per_million: 0.15,
            output_per_million: 0.60,
        }),
        "gpt-4o" => Some(TokenPrices {
            input_per_million: 2.50,
            output_per_million: 10.00,
        }),
        _ => None,
    }
}
