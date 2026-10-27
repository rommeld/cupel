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
            (
                "anthropic/claude-fable-5.1",
                Provider::OPENROUTER,
                Api::OPENAI_COMPLETIONS,
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
    fn openrouter_default_stays_qwen38_max() {
        let first = builtin_models()
            .into_iter()
            .find(|model| model.provider.as_str() == Provider::OPENROUTER)
            .expect("OpenRouter rows");
        assert_eq!(first.id, "qwen/qwen3.8-max-0902");
    }

    #[test]
    fn shared_fireworks_and_openrouter_models_keep_independent_metadata() {
        use crate::types::InputModality;

        let models = builtin_models();
        for (fireworks_id, openrouter_id, fireworks_limits, openrouter_limits, input, maps) in [
            (
                "accounts/fireworks/models/minimax-m3p1-flash",
                "minimax/minimax-m3.1-flash",
                (1_000_000, 512_000),
                (1_048_576, 524_288),
                vec![InputModality::Text, InputModality::Image],
                [
                    serde_json::json!({"minimal": null, "off": null}),
                    serde_json::json!({"minimal": null, "off": null}),
                ],
            ),
            (
                "accounts/fireworks/models/qwen3p8-2p4t-a95b",
                "qwen/qwen3.8-2.4t-a95b",
                (262_144, 131_072),
                (1_048_576, 909_000),
                vec![InputModality::Text],
                [
                    serde_json::json!({"high": null, "max": null, "minimal": null, "off": "none"}),
                    serde_json::json!({"high": null, "max": null, "minimal": null, "off": null}),
                ],
            ),
        ] {
            for (id, provider, url, limits, map) in [
                (
                    fireworks_id,
                    Provider::FIREWORKS,
                    "https://api.fireworks.ai/inference/v1",
                    fireworks_limits,
                    &maps[0],
                ),
                (
                    openrouter_id,
                    Provider::OPENROUTER,
                    "https://openrouter.ai/api/v1",
                    openrouter_limits,
                    &maps[1],
                ),
            ] {
                let model = models.iter().find(|model| model.id == id).expect(id);
                assert_eq!(model.provider.as_str(), provider, "{id}");
                assert_eq!(model.api.as_str(), Api::OPENAI_COMPLETIONS, "{id}");
                assert_eq!(model.base_url, url, "{id}");
                assert!(model.reasoning, "{id}");
                assert_eq!(model.input, input, "{id}");
                assert_eq!((model.context_window, model.max_tokens), limits, "{id}");
                assert_eq!(
                    serde_json::to_value(&model.thinking_level_map).expect("map serializes"),
                    *map,
                    "{id}"
                );
            }
        }
    }

    #[test]
    fn expanded_openrouter_families_are_available() {
        let models = builtin_models();
        for id in [
            "qwen/qwen3.8-max-prime",
            "qwen/qwen3-coder-next",
            "z-ai/glm-5.3-prime",
            "z-ai/glm-5.3-flashx",
            "anthropic/claude-fable-5.1",
            "prism-ml/ternary-bonsai-2-9b",
            "prism-ml/ternary-bonsai-2-27b",
            "inclusionai/ling-3.1-flash",
            "aion-labs/aion-3.5",
            "upstage/solar-mini4",
            "stepfun/step-5-preview",
            "sakana/fugu-ultra-v2",
            "nex-agi/nex-n2.5-pro",
            "tencent/hy4-preview",
            "ibm-granite/granite-4.2-8b",
            "meituan/longcat-2.0",
            "cohere/command-a-plus",
            "bytedance-seed/seed-2-1-turbo",
            "perceptron/perceptron-mk1.5",
        ] {
            let model = models.iter().find(|model| model.id == id).expect(id);
            assert_eq!(model.provider.as_str(), Provider::OPENROUTER, "{id}");
            assert!(
                model.input.contains(&crate::types::InputModality::Text),
                "{id}"
            );
        }
    }

    #[test]
    fn openrouter_qwen_coder_next_does_not_enable_reasoning() {
        let model = builtin_models()
            .into_iter()
            .find(|model| model.id == "qwen/qwen3-coder-next")
            .expect("Qwen3 Coder Next");
        assert!(!model.reasoning);
        assert!(model.thinking_level_map.is_none());
    }

    #[test]
    fn openrouter_kimi31_pro_keeps_its_native_reasoning_and_image_input() {
        use crate::types::InputModality;

        let model = builtin_models()
            .into_iter()
            .find(|m| m.id == "moonshotai/kimi-k3.1-pro")
            .expect("Kimi K3.1 Pro row");
        assert_eq!(model.name, "Kimi K3.1 Pro");
        assert_eq!(model.provider.as_str(), Provider::OPENROUTER);
        assert!(model.reasoning);
        assert_eq!(model.input, vec![InputModality::Text, InputModality::Image]);
        assert_eq!(
            serde_json::to_value(&model.thinking_level_map).expect("map serializes"),
            serde_json::json!({
                "medium": null,
                "minimal": null,
                "off": "none",
                "xhigh": null,
            })
        );
        assert_eq!(
            model
                .compat
                .as_ref()
                .and_then(|c| c.get("supportsTemperature")),
            Some(&serde_json::json!(false))
        );
    }

    #[test]
    fn openrouter_flint_free_has_no_token_charges() {
        use crate::types::InputModality;

        let model = builtin_models()
            .into_iter()
            .find(|m| m.id == "recursive/flint-v1.0:free")
            .expect("Flint free row");
        assert_eq!(model.provider.as_str(), Provider::OPENROUTER);
        assert!(model.reasoning);
        assert_eq!(model.input, vec![InputModality::Text]);
        for price in [
            model.cost.input,
            model.cost.output,
            model.cost.cached_read,
            model.cost.cached_write,
        ] {
            assert!(price.abs() < f64::EPSILON);
        }
        assert!(model.cost.tiers.is_none());
    }

    #[test]
    fn openrouter_free_variants_have_no_token_charges() {
        for model in builtin_models().into_iter().filter(|model| {
            model.provider.as_str() == Provider::OPENROUTER && model.id.ends_with(":free")
        }) {
            for price in [
                model.cost.input,
                model.cost.output,
                model.cost.cached_read,
                model.cost.cached_write,
            ] {
                assert!(price.abs() < f64::EPSILON, "{}", model.id);
            }
            assert!(model.cost.tiers.is_none(), "{}", model.id);
        }
    }

    #[test]
    fn openrouter_deepseek_flash_uses_the_1017_snapshot_and_native_efforts() {
        use crate::types::InputModality;

        let models = builtin_models();
        assert!(
            !models
                .iter()
                .any(|m| m.id == "deepseek/deepseek-v4.1-flash")
        );
        let model = models
            .iter()
            .find(|m| m.id == "deepseek/deepseek-v4.1-flash-1017")
            .expect("DeepSeek V4.1 Flash 1017 row");
        assert_eq!(model.name, "DeepSeek V4.1 Flash 1017");
        assert_eq!(model.provider.as_str(), Provider::OPENROUTER);
        assert!(model.reasoning);
        assert_eq!(model.input, vec![InputModality::Text, InputModality::Image]);
        // Off explicitly sends "none"; low/high/max keep their own names.
        // A max key would disable that level, even if mapped to "max".
        assert_eq!(
            serde_json::to_value(&model.thinking_level_map).expect("map serializes"),
            serde_json::json!({
                "medium": null,
                "minimal": null,
                "off": "none",
                "xhigh": null,
            })
        );
    }

    #[test]
    fn price_tier_rows_plan_against_the_threshold() {
        // The long-context family: the planning window is the price-tier
        // threshold (requests never drift into 2x pricing unnoticed), the
        // documented max input is the opt-in ceiling above it. Named rows,
        // not numbers: curation.rs gives these `Window::PriceTier`, and a row
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
    fn gpt_61_terra_rows_keep_provider_specific_limits_and_efforts() {
        use crate::types::{CostTier, InputModality, ModelCost};

        let models = builtin_models();
        for (id, provider, api, window, ceiling, map) in [
            (
                "gpt-6.1-terra",
                Provider::OPENAI,
                Api::OPENAI_RESPONSES,
                272_000,
                922_000,
                serde_json::json!({"minimal": null, "off": "none"}),
            ),
            (
                "openai/gpt-6.1-terra",
                Provider::OPENROUTER,
                Api::OPENAI_COMPLETIONS,
                272_000,
                922_000,
                serde_json::json!({"minimal": null, "off": "none"}),
            ),
            (
                "codex/gpt-6.1-terra",
                Provider::OPENAI_CODEX,
                Api::OPENAI_CODEX_RESPONSES,
                500_000,
                872_000,
                serde_json::json!({"minimal": "low", "off": null}),
            ),
        ] {
            let model = models.iter().find(|m| m.id == id).expect(id);
            assert_eq!(model.name, "GPT-6.1 Terra", "{id}");
            assert_eq!(model.provider.as_str(), provider, "{id}");
            assert_eq!(model.api.as_str(), api, "{id}");
            assert!(model.reasoning, "{id}");
            assert_eq!(model.input, vec![InputModality::Text, InputModality::Image]);
            assert_eq!(model.context_window, window, "{id}");
            assert_eq!(model.max_context_window, Some(ceiling), "{id}");
            assert_eq!(model.max_tokens, 128_000, "{id}");
            assert_eq!(
                serde_json::to_value(&model.thinking_level_map).expect("map serializes"),
                map,
                "{id}"
            );
            assert_eq!(
                model.compat.as_ref().expect("compat")["supportsTemperature"],
                false,
                "{id}"
            );
            assert_eq!(
                model.cost,
                ModelCost {
                    input: 1.0,
                    output: 5.0,
                    cached_read: 0.05,
                    cached_write: 1.25,
                    tiers: Some(vec![CostTier {
                        context_over: 272_000,
                        input: 2.0,
                        output: 7.5,
                        cached_read: 0.1,
                        cached_write: 2.5,
                    }]),
                },
                "{id}"
            );
        }
    }

    #[test]
    fn openrouter_terra_pro_keeps_its_full_window_and_always_on_reasoning() {
        use crate::types::InputModality;

        let model = builtin_models()
            .into_iter()
            .find(|model| model.id == "openai/gpt-6.1-terra-pro")
            .expect("OpenRouter Terra Pro");
        assert_eq!(model.name, "GPT-6.1 Terra Pro");
        assert_eq!(model.provider.as_str(), Provider::OPENROUTER);
        assert_eq!(model.api.as_str(), Api::OPENAI_COMPLETIONS);
        assert!(model.input.contains(&InputModality::Text));
        assert!(model.input.contains(&InputModality::Image));
        assert!(model.reasoning);
        assert_eq!(model.context_window, 272_000);
        assert_eq!(model.max_context_window, Some(1_050_000));
        assert_eq!(model.max_tokens, 128_000);
        assert_eq!(
            serde_json::to_value(&model.thinking_level_map).expect("map serializes"),
            serde_json::json!({"minimal": null, "off": null})
        );
        let compat = model.compat.expect("OpenRouter compat");
        assert_eq!(compat["supportsTemperature"], false);
        assert_eq!(compat["thinkingFormat"], "openrouter");
    }

    #[test]
    fn gpt_61_sol_ultrafast_rows_follow_sol_at_six_times_the_price() {
        use crate::types::{CostTier, ModelCost};

        let models = builtin_models();
        for base_id in ["gpt-6.1-sol", "codex/gpt-6.1-sol"] {
            // The twin sits directly behind its base row, so /model lists
            // the two speeds next to each other.
            let at = models.iter().position(|m| m.id == base_id).expect(base_id);
            let (base, twin) = (&models[at], &models[at + 1]);
            assert_eq!(twin.id, format!("{base_id}-ultrafast"));
            assert_eq!(twin.name, "GPT-6.1 Sol Ultrafast");
            // Same model on the wire; only the tier field is new.
            let compat = twin.compat.as_ref().expect("compat");
            assert_eq!(compat["requestModel"], "gpt-6.1-sol", "{base_id}");
            assert_eq!(compat["serviceTier"], "ultrafast", "{base_id}");
            assert_eq!(compat["supportsTemperature"], false, "{base_id}");
            // OpenAI's Ultrafast price table (checked 2026-10-09).
            assert_eq!(
                twin.cost,
                ModelCost {
                    input: 12.0,
                    output: 60.0,
                    cached_read: 0.6,
                    cached_write: 15.0,
                    tiers: Some(vec![CostTier {
                        context_over: 272_000,
                        input: 24.0,
                        output: 90.0,
                        cached_read: 1.2,
                        cached_write: 30.0,
                    }]),
                },
                "{base_id}"
            );
            // Limits and the effort scale are the base model's.
            assert_eq!(twin.api, base.api);
            assert_eq!(twin.provider, base.provider);
            assert_eq!(twin.context_window, base.context_window);
            assert_eq!(twin.max_context_window, base.max_context_window);
            assert_eq!(twin.thinking_level_map, base.thinking_level_map);
        }
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
        // - Ember-1 has Kimi K3's shape: none/low/high/max efforts.
        let models = builtin_models();
        for (id, map) in [
            ("xiaomi/mimo-v2.6-pro", serde_json::Value::Null),
            ("xiaomi/mimo-v2.6-flash", serde_json::Value::Null),
            (
                "fireworks/ember-1",
                serde_json::json!({
                    "off": "none", "minimal": null, "medium": null, "xhigh": null,
                }),
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
        // space gives the bare GPT-6 ids to the openai provider. They use
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
        assert_eq!(seen, 6, "only supported Codex models belong in the catalog");
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
        // default; users would see a load warning for something they never wrote.
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
