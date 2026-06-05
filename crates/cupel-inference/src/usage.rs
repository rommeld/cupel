use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: Option<u64>,
    pub cached_output_tokens: Option<u64>,

    /// Optional provider-reported reasoning token count.
    ///
    /// `OpenAi` Responses reports this as:
    ///
    /// `usage.output_tokens_details.reasoning_tokens`.
    ///
    /// Treat as a breakdown of `output_tokens`, not an additional total.
    /// Cost estimation should continue to price `output_tokens` once.
    pub reasoning_tokens: Option<u64>,
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

        // Reasoning tokens are already included in output_tokens for OpenAI
        // Responses usage. reasoning_tokens are not added here again.
        let output_cost = self.output_tokens as f64 / 1_000_000.0 * pricing.output_per_million;

        UsageCost {
            input_cost: uncached_input_cost + cached_input_cost,
            output_cost,
            total_cost: uncached_input_cost + cached_input_cost + output_cost,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[expect(clippy::float_cmp, reason = "For easier comparison.")]
    fn estimate_cost_does_not_double_charge_reasoning_tokens() {
        let usage = TokenUsage {
            input_tokens: 1_000,
            output_tokens: 2_000,
            cached_input_tokens: None,
            cached_output_tokens: None,
            reasoning_tokens: Some(500),
        };

        let cost = usage.estimate_cost(TokenPricing {
            input_per_million: 1.0,
            output_per_million: 10.0,
            cached_input_per_million: None,
        });

        // 2,000 output tokens at $10 / 1M tokens is $0.02.
        // The 500 reasoning tokens are already inside output_tokens, so they
        // must not add another charge.
        assert_eq!(cost.output_cost, 0.02);
    }
}
