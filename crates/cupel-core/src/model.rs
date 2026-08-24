//! Model registry helpers and cost/thinking-level logic.

use crate::types::{CostTier, Model, ModelThinkingLevel, Usage};

const PER_M: f64 = 1_000_000.0;

/// Fill in `usage.cost` from a model's per-million pricing.
pub(crate) fn calculate_cost(model: &Model, usage: &mut Usage) {
    let long_write = usage.cache_write1h.unwrap_or(0) as f64;
    let short_write = usage.cache_write as f64 - long_write;

    // Long-context tiers (pi: cost.tiers/inputTokensAbove): the highest
    // tier below the total prompt size reprices the WHOLE request.
    let rates = effective_rates(&model.cost, usage);

    usage.cost.input = rates.input / PER_M * usage.input as f64;
    usage.cost.output = rates.output / PER_M * usage.output as f64;
    usage.cost.cache_read = rates.cached_read / PER_M * usage.cache_read as f64;
    // 1h writes cost 2x base input; short writes use the cache-write rate.
    // The division by tokens-per-million applies to the WHOLE sum.
    usage.cost.cache_write =
        (rates.cached_write * short_write + rates.input * 2.0 * long_write) / PER_M;
    usage.cost.total =
        usage.cost.input + usage.cost.output + usage.cost.cache_read + usage.cost.cache_write;
}

/// Per-million rates for this request: the matching tier's prices, or
/// the model's base rates when no tier applies.
fn effective_rates(cost: &crate::types::ModelCost, usage: &Usage) -> CostTier {
    let base = CostTier {
        context_over: 0,
        input: cost.input,
        output: cost.output,
        cached_read: cost.cached_read,
        cached_write: cost.cached_write,
    };
    let prompt_tokens = usage.input + usage.cache_read + usage.cache_write;
    cost.tiers
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter(|tier| tier.context_over < prompt_tokens)
        .max_by_key(|tier| tier.context_over)
        .cloned()
        .unwrap_or(base)
}

const EXTENDED: [ModelThinkingLevel; 6] = [
    ModelThinkingLevel::Off,
    ModelThinkingLevel::Minimal,
    ModelThinkingLevel::Low,
    ModelThinkingLevel::Medium,
    ModelThinkingLevel::High,
    ModelThinkingLevel::XHigh,
];

/// By model supported thinking level.
#[must_use]
pub(crate) fn supported_thinking_levels(model: &Model) -> Vec<ModelThinkingLevel> {
    if !model.reasoning {
        return vec![ModelThinkingLevel::Off];
    }
    EXTENDED
        .iter()
        .copied()
        .filter(|level| {
            let entry = model
                .thinking_level_map
                .as_ref()
                .and_then(|m| m.get(level.as_str()));
            match entry {
                Some(None) => false,
                other => {
                    if *level == ModelThinkingLevel::XHigh {
                        other.is_none()
                    } else {
                        true
                    }
                }
            }
        })
        .collect()
}

/// Snap a requested level to the nearest supported one: try the exact
/// level, then walk upward, then downward.
#[must_use]
pub(crate) fn clamp_thinking_level(model: &Model, level: ModelThinkingLevel) -> ModelThinkingLevel {
    let available = supported_thinking_levels(model);
    if available.contains(&level) {
        return level;
    }
    let requested_idx = match EXTENDED.iter().position(|l| *l == level) {
        Some(i) => i,
        None => {
            return available
                .first()
                .copied()
                .unwrap_or(ModelThinkingLevel::Off);
        }
    };
    if let Some(candidates) = EXTENDED.get(requested_idx..) {
        for candidate in candidates {
            if available.contains(candidate) {
                return *candidate;
            }
        }
    }
    if let Some(candidates) = EXTENDED.get(..requested_idx) {
        for candidate in candidates.iter().rev() {
            if available.contains(candidate) {
                return *candidate;
            }
        }
    }
    available
        .first()
        .copied()
        .unwrap_or(ModelThinkingLevel::Off)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Api, CostTier, InputModality, ModelCost, Provider};

    fn tiered_model() -> Model {
        Model {
            id: "gpt-5.6-luna".into(),
            name: "GPT-5.6 Luna".into(),
            api: Api::from(Api::OPENAI_RESPONSES),
            provider: Provider::from("openai"),
            base_url: "https://api.openai.com/v1".into(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![InputModality::Text],
            cost: ModelCost {
                input: 2.0,
                output: 10.0,
                cached_read: 0.2,
                cached_write: 2.0,
                tiers: Some(vec![CostTier {
                    context_over: 272_000,
                    input: 4.0,
                    output: 15.0,
                    cached_read: 0.4,
                    cached_write: 4.0,
                }]),
            },
            context_window: 400_000,
            max_tokens: 128_000,
            headers: None,
            compat: None,
        }
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-12,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn below_threshold_uses_base_rates() {
        let model = tiered_model();
        let mut usage = Usage {
            input: 100_000,
            output: 1_000,
            ..Usage::default()
        };
        calculate_cost(&model, &mut usage);
        assert_close(usage.cost.input, 2.0 * 100_000.0 / PER_M);
        assert_close(usage.cost.output, 10.0 * 1_000.0 / PER_M);
    }

    #[test]
    fn above_272k_reprices_the_whole_request() {
        let model = tiered_model();
        let mut usage = Usage {
            input: 300_000,
            output: 2_000,
            ..Usage::default()
        };
        calculate_cost(&model, &mut usage);
        // Tier rates apply to ALL tokens, not just those past the line.
        assert_close(usage.cost.input, 4.0 * 300_000.0 / PER_M);
        assert_close(usage.cost.output, 15.0 * 2_000.0 / PER_M);
        assert_close(usage.cost.total, usage.cost.input + usage.cost.output);
    }

    #[test]
    fn cached_tokens_count_toward_the_threshold() {
        let model = tiered_model();
        let mut usage = Usage {
            input: 200_000,
            cache_read: 100_000,
            output: 1_000,
            ..Usage::default()
        };
        calculate_cost(&model, &mut usage);
        // 200k input + 100k cache reads cross 272k together.
        assert_close(usage.cost.input, 4.0 * 200_000.0 / PER_M);
        assert_close(usage.cost.cache_read, 0.4 * 100_000.0 / PER_M);
    }
}
