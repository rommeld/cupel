//! Provider-neutral inference primitives for cupel.
//!
//! This crate ownes model API access, streaming events, model metadata,
//! provider registration, and provider-neutral request/response types.
pub mod client;
pub mod context;
pub mod error;
pub mod event;
pub mod model;
pub mod provider;
pub mod providers;
pub mod registry;
pub mod tool;
pub mod usage;

pub use client::InferenceClient;
pub use context::{
    AssistantMessage, ContentBlock, InferenceContext, Message, Role, SystemMessage,
    ToolResultMessage, UserMessage,
};
pub use error::InferenceError;
pub use event::{
    AssistantMessageDelta, AssistantMessageEvent, FinishReason, InferenceStream, ToolCallDelta,
};
pub use model::{
    ApiFamily, ContextWindow, ModelId, ModelRef, ModelSpec, ProviderId, ReasoningSupport,
};
pub use provider::{InferenceProvider, InferenceRequest, InferenceRequestOptions};
pub use registry::{ModelRegistry, ProviderRegistry};
pub use tool::{JsonSchema, ToolDefinition, ToolName};
pub use usage::{TokenPricing, TokenUsage, UsageCost};

#[cfg(feature = "provider-faux")]
pub use providers::faux;

#[cfg(feature = "provider-openai-compat")]
pub use providers::openai_compat;

#[cfg(feature = "provider-openai-responses")]
pub use providers::openai_responses;