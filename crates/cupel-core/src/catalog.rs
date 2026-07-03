//! A small built-in model catalog.
//!
//! pi ships a large *generated* catalog (`models.generated.ts`, produced from
//! provider metadata). Generating that is out of scope for the first
//! iteration, so we hand-maintain a few known-good models per provider.
//! Prices are USD per million tokens and are a snapshot - treat them as
//! defaults, not truth; callers can always register their own [`Model`]s in
//! the [`ModelRegistry`](crate::model::ModelRegistry).

use std::collections::BTreeMap;

use crate::types::{Api, InputModality, Model, ModelCost, Provider, ThinkingLevelMap};

fn thinking_levels(pairs: &[(&str, Option<&str>)]) -> ThinkingLevelMap {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), v.map(str::to_string)))
        .collect::<BTreeMap<_, _>>()
}

/// Compact constructor for Fireworks models served over their
/// Anthropic-compatible endpoint. All of them share the same base URL and
/// the same compat quirks, so only the distinguishing fields are parameters.
/// Costs are (input, output, `cache_read`) in USD per million tokens -
/// Fireworks doesn't bill cache writes.
#[allow(clippy::too_many_arguments)]
fn fireworks_anthropic(
    id: &str,
    name: &str,
    vision: bool,
    input_cost: f64,
    output_cost: f64,
    cache_read_cost: f64,
    context_window: u64,
    max_tokens: u64,
) -> Model {
    Model {
        id: id.to_string(),
        name: name.to_string(),
        api: Api::from(Api::ANTHROPIC_MESSAGES),
        provider: Provider::from(Provider::FIREWORKS),
        base_url: "https://api.fireworks.ai/inference".to_string(),
        reasoning: true,
        thinking_level_map: None,
        input: if vision {
            vec![InputModality::Text, InputModality::Image]
        } else {
            vec![InputModality::Text]
        },
        cost: ModelCost {
            input: input_cost,
            output: output_cost,
            cached_read: cache_read_cost,
            cached_write: 0.0,
        },
        context_window,
        max_tokens,
        headers: None,
        // Same quirks pi records for Fireworks' Anthropic-compatible
        // endpoint: session affinity helps cache routing; eager tool input
        // streaming, cache_control on tools, and 1h cache TTL are missing.
        compat: Some(serde_json::json!({
            "sendSessionAffinityHeaders": true,
            "supportsEagerToolInputStreaming": false,
            "supportsCacheControlOnTools": false,
            "supportsLongCacheRetention": false,
        })),
    }
}

/// GLM 5.2 on Fireworks speaks Chat Completions (not the Anthropic
/// endpoint) with its own level -> effort table.
fn fireworks_glm52(
    id: &str,
    name: &str,
    input_cost: f64,
    output_cost: f64,
    cache_read_cost: f64,
) -> Model {
    Model {
        id: id.to_string(),
        name: name.to_string(),
        api: Api::from(Api::OPENAI_COMPLETIONS),
        provider: Provider::from(Provider::FIREWORKS),
        base_url: "https://api.fireworks.ai/inference/v1".to_string(),
        reasoning: true,
        // GLM 5.2's effort scale: "off" maps to none, minimal is
        // unsupported (null -> clamped away), low/medium collapse to high,
        // xhigh maps to Fireworks' "max".
        thinking_level_map: Some(thinking_levels(&[
            ("off", Some("none")),
            ("minimal", None),
            ("low", Some("high")),
            ("medium", Some("high")),
            ("xhigh", Some("max")),
        ])),
        input: vec![InputModality::Text],
        cost: ModelCost {
            input: input_cost,
            output: output_cost,
            cached_read: cache_read_cost,
            cached_write: 0.0,
        },
        context_window: 1_048_575,
        max_tokens: 131_072,
        headers: None,
        compat: Some(serde_json::json!({
            "supportsStore": false,
            "supportsDeveloperRole": false,
        })),
    }
}

/// The models cupel knows out of the box.
#[must_use]
pub fn builtin_models() -> Vec<Model> {
    let mut models = vec![
        // ------------------------------------------------------------------
        // Anthropic (direct API)
        // ------------------------------------------------------------------
        Model {
            id: "claude-sonnet-4-5".to_string(),
            name: "Claude Sonnet 4.5".to_string(),
            api: Api::from(Api::ANTHROPIC_MESSAGES),
            provider: Provider::from(Provider::ANTHROPIC),
            base_url: "https://api.anthropic.com".to_string(),
            reasoning: true,
            // Budget-based thinking; xhigh unsupported (entry -> null).
            thinking_level_map: Some(thinking_levels(&[("xhigh", None)])),
            input: vec![InputModality::Text, InputModality::Image],
            cost: ModelCost {
                input: 3.0,
                output: 15.0,
                cached_read: 0.30,
                cached_write: 3.75,
            },
            context_window: 200_000,
            max_tokens: 64_000,
            headers: None,
            compat: None,
        },
        Model {
            id: "claude-opus-4-5".to_string(),
            name: "Claude Opus 4.5".to_string(),
            api: Api::from(Api::ANTHROPIC_MESSAGES),
            provider: Provider::from(Provider::ANTHROPIC),
            base_url: "https://api.anthropic.com".to_string(),
            reasoning: true,
            thinking_level_map: Some(thinking_levels(&[("xhigh", None)])),
            input: vec![InputModality::Text, InputModality::Image],
            cost: ModelCost {
                input: 5.0,
                output: 25.0,
                cached_read: 0.50,
                cached_write: 6.25,
            },
            context_window: 200_000,
            max_tokens: 64_000,
            headers: None,
            compat: None,
        },
        Model {
            id: "claude-haiku-4-5".to_string(),
            name: "Claude Haiku 4.5".to_string(),
            api: Api::from(Api::ANTHROPIC_MESSAGES),
            provider: Provider::from(Provider::ANTHROPIC),
            base_url: "https://api.anthropic.com".to_string(),
            reasoning: true,
            thinking_level_map: Some(thinking_levels(&[("xhigh", None)])),
            input: vec![InputModality::Text, InputModality::Image],
            cost: ModelCost {
                input: 1.0,
                output: 5.0,
                cached_read: 0.10,
                cached_write: 1.25,
            },
            context_window: 200_000,
            max_tokens: 64_000,
            headers: None,
            compat: None,
        },
        // ------------------------------------------------------------------
        // OpenAI (Responses API)
        // ------------------------------------------------------------------
        Model {
            id: "gpt-5.1".to_string(),
            name: "GPT-5.1".to_string(),
            api: Api::from(Api::OPENAI_RESPONSES),
            provider: Provider::from(Provider::OPENAI),
            base_url: "https://api.openai.com/v1".to_string(),
            reasoning: true,
            // GPT-5.1 accepts effort "none" when reasoning is off.
            thinking_level_map: Some(thinking_levels(&[
                ("off", Some("none")),
                ("minimal", Some("low")),
                ("xhigh", None),
            ])),
            input: vec![InputModality::Text, InputModality::Image],
            cost: ModelCost {
                input: 1.25,
                output: 10.0,
                cached_read: 0.125,
                cached_write: 0.0,
            },
            context_window: 400_000,
            max_tokens: 128_000,
            headers: None,
            compat: None,
        },
        // ------------------------------------------------------------------
        // AWS Bedrock (ConverseStream)
        // ------------------------------------------------------------------
        Model {
            // A cross-region inference profile id; region resolution happens
            // in the provider (see providers::bedrock::build_client).
            id: "us.anthropic.claude-sonnet-4-5-20250929-v1:0".to_string(),
            name: "Claude Sonnet 4.5 (Bedrock)".to_string(),
            api: Api::from(Api::BEDROCK_CONVERSE_STREAM),
            provider: Provider::from(Provider::AMAZON_BEDROCK),
            // Empty: let the AWS SDK derive the endpoint from the region.
            base_url: String::new(),
            reasoning: true,
            thinking_level_map: Some(thinking_levels(&[("xhigh", None)])),
            input: vec![InputModality::Text, InputModality::Image],
            cost: ModelCost {
                input: 3.0,
                output: 15.0,
                cached_read: 0.30,
                cached_write: 3.75,
            },
            context_window: 200_000,
            max_tokens: 64_000,
            headers: None,
            compat: None,
        },
    ];

    // ----------------------------------------------------------------------
    // Fireworks (https://fireworks.ai) - open-weight models. Mirrors pi's
    // generated fireworks.models.ts snapshot: fourteen models on the
    // Anthropic-compatible endpoint, two (GLM 5.2) on Chat Completions.
    // ----------------------------------------------------------------------
    #[rustfmt::skip]
    models.extend([
        //                   id                                                name                    vision  in     out   cache$  context    max_out
        fireworks_anthropic("accounts/fireworks/models/deepseek-v4-flash",    "DeepSeek V4 Flash",    false,  0.14,  0.28, 0.028, 1_000_000, 384_000),
        fireworks_anthropic("accounts/fireworks/models/deepseek-v4-pro",      "DeepSeek V4 Pro",      false,  1.74,  3.48, 0.145, 1_000_000, 384_000),
        fireworks_anthropic("accounts/fireworks/models/glm-5p1",              "GLM 5.1",              false,  1.4,   4.4,  0.26,    202_800, 131_072),
        fireworks_anthropic("accounts/fireworks/models/gpt-oss-120b",         "GPT OSS 120B",         false,  0.15,  0.6,  0.015,   131_072,  32_768),
        fireworks_anthropic("accounts/fireworks/models/gpt-oss-20b",          "GPT OSS 20B",          false,  0.07,  0.3,  0.035,   131_072,  32_768),
        fireworks_anthropic("accounts/fireworks/models/kimi-k2p6",            "Kimi K2.6",            true,   0.95,  4.0,  0.16,    262_000, 262_000),
        fireworks_anthropic("accounts/fireworks/models/kimi-k2p7-code",       "Kimi K2.7 Code",       true,   0.95,  4.0,  0.19,    262_000, 262_000),
        fireworks_anthropic("accounts/fireworks/models/minimax-m2p7",         "MiniMax-M2.7",         false,  0.3,   1.2,  0.06,    196_608, 196_608),
        fireworks_anthropic("accounts/fireworks/models/minimax-m3",           "MiniMax-M3",           false,  0.3,   1.2,  0.06,    512_000, 512_000),
        fireworks_anthropic("accounts/fireworks/models/qwen3p7-plus",         "Qwen 3.7 Plus",        true,   0.4,   1.6,  0.08,    262_144,  65_536),
        fireworks_anthropic("accounts/fireworks/routers/glm-5p1-fast",        "GLM 5.1 Fast",         false,  2.8,   8.8,  0.52,    202_800, 131_072),
        fireworks_anthropic("accounts/fireworks/routers/kimi-k2p6-fast",      "Kimi K2.6 Fast",       true,   2.0,   8.0,  0.3,     262_000, 262_000),
        fireworks_anthropic("accounts/fireworks/routers/kimi-k2p6-turbo",     "Kimi K2.6 Turbo",      true,   2.0,   8.0,  0.3,     262_000, 262_000),
        fireworks_anthropic("accounts/fireworks/routers/kimi-k2p7-code-fast", "Kimi K2.7 Code Fast",  true,   1.9,   8.0,  0.38,    262_000, 262_000),
        fireworks_glm52("accounts/fireworks/models/glm-5p2",       "GLM 5.2",      1.4, 4.4, 0.26),
        fireworks_glm52("accounts/fireworks/routers/glm-5p2-fast", "GLM 5.2 Fast", 2.1, 6.6, 0.21),
    ]);

    models
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_all_sixteen_fireworks_models() {
        let fireworks: Vec<Model> = builtin_models()
            .into_iter()
            .filter(|m| m.provider.as_str() == Provider::FIREWORKS)
            .collect();
        assert_eq!(fireworks.len(), 16);
        // 14 ride the Anthropic-compatible endpoint, 2 ride Chat Completions.
        let anthropic = fireworks
            .iter()
            .filter(|m| m.api.as_str() == Api::ANTHROPIC_MESSAGES)
            .count();
        let completions = fireworks
            .iter()
            .filter(|m| m.api.as_str() == Api::OPENAI_COMPLETIONS)
            .count();
        assert_eq!((anthropic, completions), (14, 2));
    }

    #[test]
    fn every_catalog_model_has_a_registered_provider() {
        // A model whose `api` has no provider would fail at request time;
        // catch it at test time instead.
        let registry = crate::default_registry();
        for model in builtin_models() {
            assert!(
                registry.get(model.api.as_str()).is_some(),
                "no provider registered for {} (api {})",
                model.id,
                model.api
            );
        }
    }

    #[test]
    fn model_ids_are_unique() {
        let models = builtin_models();
        let mut ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "duplicate model id in catalog");
    }
}
