//! The built-in model catalog contains generated data. Do not edit by hand.
//!
//! `catalog.json` is produced by the dev-time generator
//! (`cargo run -p cupel-coding-agent --bin generate-catalog`), which
//! uses a checked-in models.dev snapshot and applies the curation tables in
//! crates/cupel-coding-agent/src/bin/generate_catalog/curation.rs
//! Add `-- --fetch` to update the snapshot. Generator tests verify that
//! snapshot plus curation produces the exact checked-in catalog bytes.
//! The JSON uses the exact same schema as a user `model.json`(a flat
//! array of camelCase [`Model`]s), so one serde derive covers both.
//! Prices are USD per million tokens; users can stll layer their own
//! models over these via ~/.cupel/model.json.

use crate::types::Model;

/// Embedded at compile time, so the runtime never touches the network or
/// the filesystem for the built-in catalog.
const CATALOG_JSON: &str = include_str!("catalog.json");

#[must_use]
pub fn builtin_models() -> Vec<Model> {
    // Invariant-backed expect: the file is generated, validated, and
    // round-trip-checked by generate-catalog and committed to git. A
    // failure here means catalog.json types::Model diverged (or the
    // file was hand-edited). Regenerate instead of editing.
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
        // and expect an Anthropic model; the ones that need a specific
        // compat blob (keyless, codex) overwrite it themselves.
        // /provider's per-provider default is first-in-catalog-order.
        let first = builtin_models().remove(0);
        assert_eq!(first.id, "claude-sonnet-5");
        assert_eq!(first.provider.as_str(), Provider::ANTHROPIC);
    }

    #[test]
    fn catalog_is_generated_and_plausible() {
        // Deliberately a floor, not an exact count: exact counts break
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
        // The invariant the old 10/2 count test was really protecting:
        // Fireworks models pair anthropic-messages with /inference and
        // openai-completions with /inference/v1, never mixed up.
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
        // at the first match: the same first-in-order rule /provider uses.
        let first = builtin_models()
            .into_iter()
            .find(|m| m.provider.as_str() == Provider::FIREWORKS)
            .expect("fireworks rows in catalog");
        assert_eq!(first.id, "accounts/fireworks/models/deepseek-v4p1-flash");
    }

    #[test]
    fn referenced_ids_are_present() {
        // cupel-coding-agent tests hardcode these ids (autocomplete,
        // models.json layering); removing them from curation.rs must
        // fail here with a clear message, not somewhere in the TUI tests.
        let models = builtin_models();
        for id in [
            "claude-sonnet-5",
            "claude-haiku-5-5",
            "claude-sonnet-4-5",
            "gpt-6-astra",
            "codex/gpt-6-astra",
            "openai/gpt-6-astra",
        ] {
            assert!(
                models.iter().any(|m| m.id == id),
                "{id} missing from catalog"
            );
        }
    }

    #[test]
    fn fable_51_catalog_rows_keep_the_native_effort_scale() {
        let models = builtin_models();
        for (id, provider, api) in [
            (
                "claude-fable-5-1",
                Provider::ANTHROPIC,
                Api::ANTHROPIC_MESSAGES,
            ),
            (
                "us.anthropic.claude-fable-5-1",
                Provider::AMAZON_BEDROCK,
                Api::BEDROCK_CONVERSE_STREAM,
            ),
        ] {
            let model = models.iter().find(|m| m.id == id).expect("Fable 5.1 row");
            assert_eq!(model.provider.as_str(), provider, "{id}");
            assert_eq!(model.api.as_str(), api, "{id}");
            let levels = model.thinking_level_map.as_ref().expect("effort map");
            assert_eq!(levels.get("off"), Some(&None), "{id}");
            assert_eq!(levels.get("minimal"), Some(&None), "{id}");
            assert!(
                !levels.contains_key("xhigh") && !levels.contains_key("max"),
                "{id}"
            );
        }
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

    #[test]
    fn openrouter_models_ride_the_completions_endpoint() {
        // OpenRouter is a router, not a vendor: every curated model speaks
        // plain openai-completions against the one openrouter.ai base URL,
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
    fn openai_rows_plan_against_the_price_tier() {
        // The long-context family: the planning window is the price-tier
        // threshold (requests never drift into 2x pricing unnoticed), the
        // documented max input is the opt-in ceiling above it.
        let mut seen = 0;
        for model in builtin_models() {
            if model.provider.as_str() != Provider::OPENAI {
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
        assert!(seen > 0, "no openai models in the catalog");
    }

    #[test]
    fn sol_luna_rows_switch_off_where_the_scale_has_none() {
        // GPT-6 Sol and Luna share Astra's shape (no temperature, xhigh
        // and max selectable) with one difference: models.dev lists
        // "none" on their effort scale, so off is sent as effort "none".
        // The Codex backend's scale has no none, so off stays disabled.
        let models = builtin_models();
        for id in [
            "gpt-6-sol",
            "gpt-6-luna",
            "openai/gpt-6-sol",
            "openai/gpt-6-luna",
            "codex/gpt-6-sol",
            "codex/gpt-6-luna",
        ] {
            let model = models.iter().find(|m| m.id == id).expect(id);
            assert_eq!(
                model
                    .compat
                    .as_ref()
                    .and_then(|c| c.get("supportsTemperature")),
                Some(&serde_json::json!(false)),
                "{id}"
            );
            let map = model.thinking_level_map.as_ref().expect("map");
            let off = if id.starts_with("codex/") {
                None
            } else {
                Some("none".to_string())
            };
            assert_eq!(map.get("off"), Some(&off), "{id}");
            assert!(
                !map.contains_key("xhigh"),
                "{id}: xhigh key would DISABLE it"
            );
            assert!(!map.contains_key("max"), "{id}: max key would DISABLE it");
        }
    }

    #[test]
    fn gpt_61_sol_rows_cannot_switch_off() {
        // Unlike Sol, its effort scale has no "none" (OpenAI's
        // model page: "none" and "minimal" are not supported). So off ->
        // null in every dialect: the provider leaves `reasoning` out and
        // never sends the unsupported effort "none".
        let models = builtin_models();
        for (id, api) in [
            ("gpt-6.1-sol", Api::OPENAI_RESPONSES),
            ("openai/gpt-6.1-sol", Api::OPENAI_COMPLETIONS),
            ("codex/gpt-6.1-sol", Api::OPENAI_CODEX_RESPONSES),
        ] {
            let model = models.iter().find(|m| m.id == id).expect(id);
            assert_eq!(model.name, "GPT-6.1 Sol");
            assert_eq!(model.api.as_str(), api);
            let map = model.thinking_level_map.as_ref().expect("effort map");
            // `Some(&None)` = the key exists with a JSON null: "off" is
            // unsupported. `Some(&Some("none"))` would send effort "none".
            assert_eq!(map.get("off"), Some(&None), "{id}");
            assert!(!map.contains_key("max"));
            assert_eq!(
                model
                    .compat
                    .as_ref()
                    .and_then(|c| c.get("supportsTemperature")),
                Some(&serde_json::json!(false))
            );
        }
        let codex = models
            .iter()
            .find(|m| m.id == "codex/gpt-6.1-sol")
            .expect("codex row");
        assert_eq!(
            codex.compat.as_ref().and_then(|c| c.get("requestModel")),
            Some(&serde_json::json!("gpt-6.1-sol"))
        );
    }

    #[test]
    fn astra_rows_keep_the_documented_effort_scale() {
        // GPT-6 Astra in all three dialects: no temperature, no off, no
        // minimal, and both top levels selectable (keys absent).
        let models = builtin_models();
        for id in ["gpt-6-astra", "openai/gpt-6-astra", "codex/gpt-6-astra"] {
            let model = models.iter().find(|m| m.id == id).expect(id);
            assert_eq!(
                model
                    .compat
                    .as_ref()
                    .and_then(|c| c.get("supportsTemperature")),
                Some(&serde_json::json!(false)),
                "{id}"
            );
            let map = model.thinking_level_map.as_ref().expect("map");
            assert_eq!(map.get("off"), Some(&None), "{id}");
            assert!(
                !map.contains_key("xhigh"),
                "{id}: xhigh key would DISABLE it"
            );
            assert!(!map.contains_key("max"), "{id}: max key would DISABLE it");
        }
        // minimal: unsupported on the API (clamps up to low), pinned to
        // "low" on Codex like every other Codex row. The wire result is the same.
        let api = models
            .iter()
            .find(|m| m.id == "gpt-6-astra")
            .expect("api row");
        assert_eq!(
            api.thinking_level_map.as_ref().expect("map").get("minimal"),
            Some(&None)
        );
    }

    #[test]
    fn opus55_row_is_adaptive_only() {
        // Claude Opus 5.5 answers `thinking: {type: "disabled"}` and
        // `budget_tokens` with a 400 at every effort level. The row must
        // take the adaptive path (effort, no temperature) and pin "off"
        // to null, so the provider omits `thinking` instead of disabling it.
        // It also runs the preserved-thinking check: drop, don't reject.
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
        // Both models answer `budget_tokens` with a 400, like Opus 5.5:
        // both rows take the adaptive path (effort, no temperature).
        // "off" is where they differ. models.dev lists a toggle for
        // Sonnet 5, so "off" gets no entry and the provider sends
        // `disabled`, which Sonnet 5 accepts. Opus 5 has no toggle:
        // off -> null, and the provider leaves `thinking` out.
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
        // 400 too: its off is the thinking type `between_tools`, pinned
        // in curation.rs. Without that entry the provider would send
        // `disabled`; with a null entry it would leave `thinking` out and
        // the model would think at its default effort, high. Like Opus 5.5
        // it runs the preserved-thinking check: drop, don't reject.
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
        // The whole map, not single keys: an extra xhigh or max key would
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
        assert!(!models.iter().any(|m| m.id == "claude-haiku-4-5"));
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
        // Haiku's toggle sends `disabled`; Sonnet's `between_tools` is a 400.
        assert_eq!(
            serde_json::to_value(&model.thinking_level_map).expect("map serializes"),
            serde_json::json!({"minimal": null})
        );
    }

    #[test]
    fn fireworks_glm53_rows_keep_the_native_effort_scale() {
        // GLM 5.3, its fast router, and 5.3 Flash ride completions with the
        // map derived from models.dev's low/high/max scale: low stays low,
        // off cannot be switched (no reasoning_effort is sent), medium and
        // xhigh clamp to their neighbours at request time, max stays absent.
        let models = builtin_models();
        for id in [
            "accounts/fireworks/models/glm-5p3",
            "accounts/fireworks/routers/glm-5p3-fast",
            "accounts/fireworks/models/glm-5p3-flash",
        ] {
            let model = models.iter().find(|m| m.id == id).expect(id);
            assert_eq!(model.api.as_str(), Api::OPENAI_COMPLETIONS, "{id}");
            let map = model.thinking_level_map.as_ref().expect("map");
            assert_eq!(map.get("off"), Some(&None), "{id}");
            assert_eq!(map.get("medium"), Some(&None), "{id}");
            assert_eq!(map.get("xhigh"), Some(&None), "{id}");
            assert!(!map.contains_key("low"), "{id}: low keeps its own name");
            assert!(!map.contains_key("max"), "{id}: max key would DISABLE it");
        }
    }

    #[test]
    fn openrouter_mimo_and_ember_keep_their_reasoning_scales() {
        // The whole map, not single keys: an extra xhigh or max key would
        // DISABLE that level. Prices and limits stay out of this test:
        // OpenRouter's prices change within hours, and the generator copies
        // them from models.dev anyway.
        // - MiMo V2.6 has only a toggle (like Laguna S 2.1): no map, so off
        //   sends effort "none" and every other level goes out by its name.
        // - Ember-1 has Kimi K3's shape: a toggle plus low/high/max.
        let models = builtin_models();
        for (id, map) in [
            ("xiaomi/mimo-v2.6-pro", serde_json::Value::Null),
            ("xiaomi/mimo-v2.6-flash", serde_json::Value::Null),
            (
                "fireworks/ember-1",
                serde_json::json!({"minimal": null, "medium": null, "xhigh": null}),
            ),
        ] {
            let model = models.iter().find(|m| m.id == id).expect(id);
            assert!(model.reasoning, "{id}");
            assert_eq!(
                serde_json::to_value(&model.thinking_level_map).expect("map serializes"),
                map,
                "{id}"
            );
        }
    }

    #[test]
    fn fireworks_inkling_thinks_through_token_budgets() {
        // models.dev lists no reasoning options for Fireworks' Inkling
        // (OpenRouter's Inkling has a none..max effort scale). The row takes
        // the Fireworks Anthropic template anyway: no map, so every level
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
        // space gives the bare gpt-5.6 ids to the openai provider. They use
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
            assert_eq!(
                model.id.strip_prefix("codex/"),
                request_model,
                "{}: id must be codex/<requestModel>",
                model.id
            );
            // The minimal -> "low" pin survives. `xhigh` stays absent so
            // cupel's key-absence rule keeps the level available.
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
        assert_eq!(seen, 8, "only supported Codex models belong in the catalog");
    }
}
