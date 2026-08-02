//! Model registry helpers and cost/thinking-level logic.

use crate::types::{Model, ModelThinkingLevel, Usage};

const PER_M: f64 = 1_000_000.0;

/// Fill in `usage.cost` from a model's per-million pricing.
pub(crate) fn calculate_cost(model: &Model, usage: &mut Usage) {
    let long_write = usage.cache_write1h.unwrap_or(0) as f64;
    let short_write = usage.cache_write as f64 - long_write;

    usage.cost.input = model.cost.input / PER_M * usage.input as f64;
    usage.cost.output = model.cost.output / PER_M * usage.output as f64;
    usage.cost.cache_read = model.cost.cached_read / PER_M * usage.cache_read as f64;
    // 1h writes cost 2x base input; short writes use the cache-write rate.
    // The division by tokens-per-million applies to the WHOLE sum.
    usage.cost.cache_write =
        (model.cost.cached_write * short_write + model.cost.input * 2.0 * long_write) / PER_M;
    usage.cost.total =
        usage.cost.input + usage.cost.output + usage.cost.cache_read + usage.cost.cache_write;
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
