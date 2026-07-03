//! Provider-neutral inference primitives for cupel.
//!
//! This crate only translates between the unified data model and provider
//! wire protocols. It should not load user config, read environment
//! variables (except through explicitly-passed options), or execute tools.
//! Those jobs belong to the agent and CLI crates.
//!
//! Module map:
//! - [`types`] - the unified data model (messages, models, options, events)
//! - [`event_stream`] - the async channel providers stream events through
//! - [`error`] - error types
//! - [`provider`] - the [`Provider`](provider::Provider) trait + registry
//! - [`providers`] - the concrete adapters (Anthropic, `OpenAI`, Bedrock)
//! - [`model`] - model registry, cost math, thinking-level clamping
//! - [`catalog`] - a small built-in model catalog
//! - [`sse`] / [`json_util`] / [`transform`] / [`options_util`] - shared
//!   plumbing used by the providers

pub mod catalog;
pub mod error;
pub mod event_stream;
pub mod json_util;
pub mod model;
pub mod options_util;
pub mod provider;
pub mod providers;
pub mod sse;
pub mod transform;
pub mod types;

use std::sync::Arc;

/// A [`provider::Registry`] with all built-in providers registered - the
/// usual entry point for applications.
#[must_use]
pub fn default_registry() -> provider::Registry {
    let mut registry = provider::Registry::new();
    registry.register(Arc::new(providers::anthropic::AnthropicProvider::new()));
    registry.register(Arc::new(
        providers::openai_responses::OpenAiResponsesProvider::new(),
    ));
    registry.register(Arc::new(
        providers::openai_completions::OpenAiCompletionsProvider::new(),
    ));
    registry.register(Arc::new(providers::bedrock::BedrockProvider::new()));
    registry
}
