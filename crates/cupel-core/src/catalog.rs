//! The built in model catalog contains generated data. Do not edit by hand.
//!
//! `catalog.json` is produced by the dev time generator
//! (the catalog generator binary), which
//! uses a checked in models.dev snapshot and applies the curation tables in
//! the generator's curation.rs module
//! Add the fetch option to update the snapshot. Generator tests verify that
//! snapshot plus curation produces the exact checked in catalog bytes.
//! The JSON uses the exact same schema as a user `model.json`(a flat
//! array of camelCase [`Model`]s), so one serde derive covers both.
//! Prices are USD per million tokens, users can stll layer their own
//! models over these via ~/.cupel/model.json.

use crate::types::Model;

/// Embedded at compile time, so the runtime never touches the network or
/// the filesystem for the built in catalog.
const CATALOG_JSON: &str = include_str!("catalog.json");

#[must_use]
pub fn builtin_models() -> Vec<Model> {
    // Invariant backed expect, the file is generated, validated, and
    // round trip checked by generate catalog and committed to git. A
    // failure here means catalog.json Model diverged (or the
    // file was hand edited). Regenerate instead of editing.
    serde_json::from_str(CATALOG_JSON).expect(
        "catalog.json is generated data; regenrate it with \
         `cargo run -p cupel-coding-agent --bin generate-catalog`",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Api, Provider};

    #[test]
    fn first_model_is_the_anthropic_default() {
        // Fixtures across the workspace call builtin_models().remove(0)
        // and expect an Anthropic model, the ones that need a specific
        // compat blob (keyless, codex) overwrite it themselves.
        // /provider's per provider default is first in catalog order.
        let first = builtin_models().remove(0);
        assert_eq!(first.id, "claude-sonnet-5");
        assert_eq!(first.provider.as_str(), Provider::ANTHROPIC);
    }

    #[test]
    fn catalog_is_generated_and_plausible() {
        // Deliberately a floor, not an exact count, exact counts break
        // on every curation edit without catching real defects.
        let models = builtin_models();
        assert!(models.len() >= 20, "suspiciously small: {}", models.len());
        for model in &models {
            assert!(
                model.context_window > 0,
                "{} has no context window",
                model.id
            );
            assert!(model.max_tokens > 0, "{} has no max_tokens", model.id);
            assert!(
                !model.input.is_empty(),
                "{} has no input modality",
                model.id
            );
        }
    }

    #[test]
    fn fireworks_models_ride_the_expected_endpoints() {
        // The invariant the old 10/2 count test was really protecting,
        // Fireworks models pair anthropic messages with /inference and
        // openai completions with /inference/v1, never mixed up.
        let mut seen = 0;
        for model in builtin_models() {
            if model.provider.as_str() != Provider::FIREWORKS {
                continue;
            }
            seen += 1;
            let pair = (model.api.as_str(), model.base_url.as_str());
            assert!(
                pair == (
                    Api::ANTHROPIC_MESSAGES,
                    "https://api.fireworks.ai/inference"
                ) || pair
                    == (
                        Api::OPENAI_COMPLETIONS,
                        "https://api.fireworks.ai/inference/v1"
                    ),
                "{} rides an unexpected endpoint: {pair:?}",
                model.id
            );
        }
        assert!(seen > 0, "no fireworks models in the catalog");
    }

    #[test]
    fn fireworks_default_is_deepseek_v41_flash() {
        // `/provider fireworks` switches to the first Fireworks row in
        // catalog order. Pinned here so that removing rows from curation.rs
        // cannot quietly promote another model to the default. `find` stops
        // at the first match, the same first in order rule /provider uses.
        let first = builtin_models()
            .into_iter()
            .find(|m| m.provider.as_str() == Provider::FIREWORKS)
            .expect("fireworks rows in catalog");
        assert_eq!(first.id, "accounts/fireworks/models/deepseek-v4p1-flash");
    }

    #[test]
    fn claude_catalog_models_are_version_five_or_newer() {
        for model in builtin_models()
            .into_iter()
            .filter(|model| model.id.contains("claude-"))
        {
            let major = model
                .id
                .split(['-', '.'])
                .find_map(|part| part.parse::<u32>().ok())
                .expect("Claude model id contains a version");
            assert!(major >= 5, "{} is a retired Claude model", model.id);
        }
    }

    #[test]
    fn openai_catalog_models_are_version_six_or_newer() {
        for model in builtin_models().into_iter().filter(|model| {
            matches!(
                model.provider.as_str(),
                Provider::OPENAI | Provider::OPENAI_CODEX
            ) || model.id.starts_with("openai/")
        }) {
            let major = model
                .id
                .split(['-', '.'])
                .find_map(|part| part.parse::<u32>().ok())
                .expect("OpenAI model id contains a version");
            assert!(major >= 6, "{} is a retired OpenAI model", model.id);
        }
    }

    #[test]
    fn fable_52_enables_preserved_thinking_workaround() {
        // Fable 5.2 rejects replayed thinking after history edits for every
        // account, so it must opt into the preserved-thinking workaround.
        let model = builtin_models()
            .into_iter()
            .find(|model| model.id == "claude-fable-5-2")
            .expect("Fable 5.2 row");
        assert_eq!(
            model
                .compat
                .as_ref()
                .and_then(|compat| compat.get("prefixMismatchBehavior")),
            Some(&serde_json::json!("drop_block"))
        );
    }

    #[test]
    fn every_catalog_model_has_a_registered_provider() {
        // A model whose `api` has no provider would fail at request time,
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

    #[test]
    fn openrouter_models_ride_the_completions_endpoint() {
        // OpenRouter is a router, not a vendor, every curated model speaks
        // plain openai completions against the one openrouter.ai base URL,
        // with the openrouter thinking format pinned in compat.
        let mut seen = 0;
        for model in builtin_models() {
            if model.provider.as_str() != Provider::OPENROUTER {
                continue;
            }
            seen += 1;
            assert_eq!(model.api.as_str(), Api::OPENAI_COMPLETIONS, "{}", model.id);
            assert_eq!(
                model.base_url, "https://openrouter.ai/api/v1",
                "{}",
                model.id
            );
            let format = model
                .compat
                .as_ref()
                .and_then(|compat| compat.get("thinkingFormat"))
                .and_then(serde_json::Value::as_str);
            assert_eq!(format, Some("openrouter"), "{}", model.id);
        }
        assert!(seen > 0, "no openrouter models in the catalog");
    }

    #[test]
    fn openrouter_default_stays_qwen38_max() {
        let first = builtin_models()
            .into_iter()
            .find(|model| model.provider.as_str() == Provider::OPENROUTER)
            .expect("OpenRouter rows");
        assert_eq!(first.id, "qwen/qwen3.8-max-0902");
    }

    #[test]
    fn price_tier_rows_plan_against_the_threshold() {
        // The long context family, the planning window is the price tier
        // threshold (requests never drift into 2x pricing unnoticed), the
        // documented max input is the opt in ceiling above it. Named rows,
        // not numbers, curation.rs gives these `PriceTier`, and a row
        // that lost it would plan against its full window again.
        let mut seen = 0;
        for model in builtin_models() {
            let plans_on_tier = model.provider.as_str() == Provider::OPENAI
                || (model.provider.as_str() == Provider::OPENROUTER
                    && model.id.starts_with("openai/"))
                || matches!(
                    model.id.as_str(),
                    "claude-haiku-5-5"
                        | "anthropic/claude-haiku-5.5"
                        | "qwen/qwen3.7-plus"
                        | "x-ai/grok-build-0.1"
                        | "sakana/fugu-ultra-v2"
                        | "bytedance-seed/seed-2.0-lite"
                        | "bytedance-seed/seed-2.0-mini"
                );
            if !plans_on_tier {
                continue;
            }
            seen += 1;
            let tier = model.cost.tiers.as_ref().and_then(|t| t.first());
            assert_eq!(
                tier.map(|t| t.context_over),
                Some(model.context_window),
                "{}: contextWindow must equal the price-tier threshold",
                model.id
            );
            assert!(
                model.max_context_window > Some(model.context_window),
                "{}: the ceiling must sit above the planning window",
                model.id
            );
        }
        assert!(seen > 0, "no price-tier rows in the catalog");
    }

    #[test]
    fn opus55_row_is_adaptive_only() {
        // Claude Opus 5.5 answers `thinking` with `type` set to `disabled` and
        // `budget_tokens` with a 400 at every effort level. The row must
        // take the adaptive path (effort, no temperature) and pin "off"
        // to null, so the provider omits `thinking` instead of disabling it.
        // It also runs the preserved thinking check, drop, don't reject.
        let models = builtin_models();
        let model = models
            .iter()
            .find(|m| m.id == "claude-opus-5-5")
            .expect("claude-opus-5-5 in catalog");
        assert_eq!(model.provider.as_str(), Provider::ANTHROPIC);
        assert_eq!(
            model.compat,
            Some(serde_json::json!({
                "forceAdaptiveThinking": true,
                "supportsTemperature": false,
                "prefixMismatchBehavior": "drop_block",
            }))
        );
        let map = model.thinking_level_map.as_ref().expect("map");
        assert_eq!(map.get("off"), Some(&None));
        assert_eq!(map.get("minimal"), Some(&None));
        assert!(!map.contains_key("xhigh"), "xhigh key would DISABLE it");
        assert!(!map.contains_key("max"), "max key would DISABLE it");
    }

    #[test]
    fn sonnet5_and_opus5_rows_are_adaptive() {
        // Both models answer `budget_tokens` with a 400, like Opus 5.5,
        // both rows take the adaptive path (effort, no temperature).
        // "off" is where they differ. models.dev lists a toggle for
        // Sonnet 5, so "off" gets no entry and the provider sends
        // `disabled`, which Sonnet 5 accepts. Opus 5 has no toggle,
        // off → null, and the provider leaves `thinking` out.
        let models = builtin_models();
        for (id, off) in [("claude-sonnet-5", None), ("claude-opus-5", Some(&None))] {
            let model = models.iter().find(|m| m.id == id).expect("row in catalog");
            assert_eq!(
                model.compat,
                Some(serde_json::json!({
                    "forceAdaptiveThinking": true,
                    "supportsTemperature": false,
                })),
                "{id}"
            );
            let map = model.thinking_level_map.as_ref().expect("map");
            assert_eq!(map.get("off"), off, "{id}");
            assert_eq!(map.get("minimal"), Some(&None), "{id}");
            assert!(
                !map.contains_key("xhigh"),
                "{id}: xhigh key would DISABLE it"
            );
            assert!(!map.contains_key("max"), "{id}: max key would DISABLE it");
        }
    }

    #[test]
    fn sonnet55_row_switches_off_with_between_tools() {
        // Claude Sonnet 5.5 takes the adaptive path like Opus 5.5 (effort,
        // no temperature, `budget_tokens` is a 400), but `disabled` is a
        // 400 too, its off is the thinking type `between_tools`, pinned
        // in curation.rs. Without that entry the provider would send
        // `disabled`, with a null entry it would leave `thinking` out and
        // the model would think at its default effort, high. Like Opus 5.5
        // it runs the preserved thinking check, drop, don't reject.
        let models = builtin_models();
        let model = models
            .iter()
            .find(|m| m.id == "claude-sonnet-5-5")
            .expect("claude-sonnet-5-5 in catalog");
        assert_eq!(model.provider.as_str(), Provider::ANTHROPIC);
        assert_eq!(
            model.compat,
            Some(serde_json::json!({
                "forceAdaptiveThinking": true,
                "supportsTemperature": false,
                "prefixMismatchBehavior": "drop_block",
            }))
        );
        // The whole map, not single keys, an extra xhigh or max key would
        // DISABLE that level.
        let map = model.thinking_level_map.as_ref().expect("map");
        assert_eq!(
            serde_json::to_value(map).expect("map serializes"),
            serde_json::json!({"minimal": null, "off": "between_tools"})
        );
    }

    #[test]
    fn haiku55_row_is_adaptive_and_keeps_its_native_toggle() {
        let models = builtin_models();
        let model = models
            .iter()
            .find(|m| m.id == "claude-haiku-5-5")
            .expect("claude-haiku-5-5 in catalog");
        assert_eq!(model.name, "Claude Haiku 5.5");
        assert_eq!(model.provider.as_str(), Provider::ANTHROPIC);
        assert_eq!(model.api.as_str(), Api::ANTHROPIC_MESSAGES);
        assert!(model.reasoning);
        assert_eq!(
            model.compat,
            Some(serde_json::json!({
                "forceAdaptiveThinking": true,
                "supportsTemperature": false,
                "prefixMismatchBehavior": "drop_block",
            }))
        );
        // Haiku's toggle sends `disabled`, Sonnet's `between_tools` is a 400.
        assert_eq!(
            serde_json::to_value(&model.thinking_level_map).expect("map serializes"),
            serde_json::json!({"minimal": null})
        );
    }

    #[test]
    fn fireworks_inkling_thinks_through_token_budgets() {
        // models.dev lists no reasoning options for Fireworks' Inkling
        // (OpenRouter's Inkling has a none..max effort scale). The row takes
        // the Fireworks Anthropic template anyway, no map, so every level
        // stays selectable, and Fireworks turns `budget_tokens` into its
        // own effort bands.
        let model = builtin_models()
            .into_iter()
            .find(|m| m.id == "accounts/fireworks/models/inkling")
            .expect("Fireworks Inkling row");
        assert_eq!(model.api.as_str(), Api::ANTHROPIC_MESSAGES);
        assert!(model.reasoning);
        assert!(model.thinking_level_map.is_none());
        assert_eq!(
            model.compat,
            Some(serde_json::json!({
                "sendSessionAffinityHeaders": true,
                "supportsEagerToolInputStreaming": false,
                "supportsCacheControlOnTools": false,
                "supportsLongCacheRetention": false,
            }))
        );
    }

    #[test]
    fn codex_models_ride_the_chatgpt_backend() {
        // The subscription rows have namespaced ids because cupel's flat id
        // space gives the bare GPT 6 ids to the openai provider. They use
        // the ChatGPT backend URL and a compat requestModel carrying the wire name
        // the namespacing hid.
        let mut seen = 0;
        for model in builtin_models() {
            if model.provider.as_str() != Provider::OPENAI_CODEX {
                continue;
            }
            seen += 1;
            assert_eq!(
                model.api.as_str(),
                Api::OPENAI_CODEX_RESPONSES,
                "{}",
                model.id
            );
            assert_eq!(
                model.base_url, "https://chatgpt.com/backend-api",
                "{}",
                model.id
            );
            assert!(model.reasoning, "{}: every codex model reasons", model.id);
            let request_model = model
                .compat
                .as_ref()
                .and_then(|compat| compat.get("requestModel"))
                .and_then(serde_json::Value::as_str);
            let id = model.id.strip_suffix("-ultrafast").unwrap_or(&model.id);
            assert_eq!(
                id.strip_prefix("codex/"),
                request_model,
                "{}: id must be codex/<requestModel>[-ultrafast]",
                model.id
            );
            // The minimal → "low" pin survives. `xhigh` stays absent so
            // cupel's key absence rule keeps the level available.
            let map = model.thinking_level_map.as_ref().expect("map pinned");
            assert_eq!(
                map.get("minimal"),
                Some(&Some("low".to_string())),
                "{}",
                model.id
            );
            assert!(
                !map.contains_key("xhigh"),
                "{}: xhigh entry would DISABLE xhigh",
                model.id
            );
        }
        assert!(seen > 0, "no Codex models in the catalog");
    }

    #[test]
    fn bedrock_claude_rows_share_the_anthropic_twins_thinking_compat() {
        // Both adapters read `forceAdaptiveThinking` from the row. A Bedrock
        // twin that drifted would think differently from the same model on
        // the Anthropic API (budget_tokens where only adaptive is accepted).
        let models = builtin_models();
        let forced = |model: &Model| {
            model
                .compat
                .as_ref()
                .and_then(|compat| compat.get("forceAdaptiveThinking"))
                .and_then(serde_json::Value::as_bool)
        };
        let mut twins = 0;
        for bedrock in models
            .iter()
            .filter(|m| m.api.as_str() == Api::BEDROCK_CONVERSE_STREAM)
        {
            let Some((_, base)) = bedrock.id.split_once("anthropic.") else {
                continue;
            };
            let Some(anthropic) = models.iter().find(|m| {
                m.provider.as_str() == Provider::ANTHROPIC
                    && m.api.as_str() == Api::ANTHROPIC_MESSAGES
                    && m.id == base
            }) else {
                continue;
            };
            assert_eq!(
                forced(bedrock),
                forced(anthropic),
                "{} vs {}",
                bedrock.id,
                anthropic.id
            );
            twins += 1;
        }
        assert!(twins > 0, "Claude rows exist on both Bedrock and Anthropic");
    }

    #[test]
    fn every_builtin_compat_blob_parses_cleanly() {
        // A shipped row with a mistyped knob would quietly run on that knob's
        // default, users would see a load warning for something they never wrote.
        let registry = crate::default_registry();
        for model in builtin_models() {
            let provider = registry.get(model.api.as_str()).expect("registered");
            assert_eq!(
                provider.compat_problems(&model),
                Vec::<String>::new(),
                "{}",
                model.id
            );
        }
    }
}
