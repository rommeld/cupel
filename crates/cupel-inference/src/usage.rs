use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: Option<u64>,
    pub cached_output_tokens: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TokenPricing {
    /// Cost per one million input tokens.
    pub input_per_million: f64,

    /// Cost per one million output tokens.
    pub output_per_million: f64,

    /// Optional lower cost for cached input tokens.
    pub cached_input_per_million: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct UsageCost {
    pub input_cost: f64,
    pub output_cost: f64,
    pub total_cost: f64,
}

impl TokenUsage {
    #[must_use]
    pub fn estimate_cost(self, pricing: TokenPricing) -> UsageCost {
        let cached_input = self.cached_input_tokens.unwrap_or(0);
        let uncached_input = self.input_tokens.saturating_sub(cached_input);

        let uncached_input_cost = uncached_input as f64 / 1_000_000.0 * pricing.input_per_million;

        let cached_input_cost = match pricing.cached_input_per_million {
            Some(price) => cached_input as f64 / 1_000_000.0 * price,
            None => cached_input as f64 / 1_000_000.0 * pricing.input_per_million,
        };

        let output_cost = self.output_tokens as f64 / 1_000_000.0 * pricing.output_per_million;

        UsageCost {
            input_cost: uncached_input_cost + cached_input_cost,
            output_cost,
            total_cost: uncached_input_cost + cached_input_cost + output_cost,
        }
    }
}
