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

/// The models cupel knows out of the box.
#[must_use]
pub fn builtin_models() -> Vec<Model> {
    vec![
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
    ]
}
