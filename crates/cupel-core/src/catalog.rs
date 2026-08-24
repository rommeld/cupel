//! The built-in model catalog - GENERATED DATA, do not edit by hand.
//!
//! `catalog.json` is produced by the dev-time generator
//! (`cargo run -p cupel-coding-agent --bin generate-catalog`), which
//! fetches models.dev and applies the curation tables in
//! crates/cupel-coding-agent/src/bin/generate_catalog/curation.rs
//! The JSON uses the exact same schema as a user `model.json`(a flat
//! array of camelCase [`Model`]s), so one serde derive covers both.
//! Prices are USD per million tokens; users can stll layer their own
//! models over these via ~/.cupel/model.json.

use crate::types::Model;

/// Embedded at compile time - the runtime never touches the network or
/// the filesystem for the built-in catalog.
const CATALOG_JSON: &str = include_str!("catalog.json");

#[must_use]
pub fn builtin_models() -> Vec<Model> {
    // Invariant-backed expect: the file is generated, validated, and
    // round-trip-checked by generate-catalog and committed to git. A
    // failure here means catalog.json types::Model diverged (or the
    // file was hand-edited) - regenerate instead of editing.
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
        // and expect "a plain Anthropic model with no compat blob";
        // /provider's per-provider default is first-in-catalog-order.
        let first = builtin_models().remove(0);
        assert_eq!(first.id, "claude-sonnet-5");
        assert_eq!(first.provider.as_str(), Provider::ANTHROPIC);
        assert!(first.compat.is_none());
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
        // openai-completions with /inference/v1 - never mixed up.
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
    fn referenced_ids_are_present() {
        // cupel-coding-agent tests hardcode these ids (autocomplete,
        // models.json layering); removing them from curation.rs must
        // fail HERE with a clear message, not somewhere in the TUI tests.
        let models = builtin_models();
        for id in ["claude-sonnet-5", "claude-haiku-4-5", "claude-sonnet-4-5"] {
            assert!(
                models.iter().any(|m| m.id == id),
                "{id} missing from catalog"
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
}
