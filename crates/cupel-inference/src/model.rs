use std::fmt;

use serde::{Deserialize, Serialize};

use crate::usage::TokenPricing;

/// Stable internal identifier for a model entry in Cupel's model registry.
///
/// Example:
/// - "openai/gpt-5.5"
/// - "anthropic/claud-sonnet-4.8"
/// - "local/qwen-coder"
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelRef(pub String);

/// Provider-facing model identifier.
///
/// Actual value sent to the provider API.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelId(pub String);

/// Stable provider identifier.
///
/// Examples:
/// - "openai"
/// - "anthropic"
/// - "google"
/// - "ollama"
/// - "litellm"
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProviderId(pub String);

/// API protocol family.
///
/// This is deliberately not the same thing as a provider name.
/// Several providers can expose the same API family.
///
/// Examples:
/// - "fireworks" -> "anthropic-messages"
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApiFamily {
    OpenAiChatCompletions,
    OpenAiResponses,
    AnthropicMessages,
    MistralConversations,
    BedrockConverseStream,
}

impl fmt::Display for ApiFamily {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match *self {
            Self::OpenAiChatCompletions => "openai-chat-completions",
            Self::OpenAiResponses => "openai-responses",
            Self::AnthropicMessages => "anthropic-messages",
            Self::MistralConversations => "mistral-conversations",
            Self::BedrockConverseStream => "bedrock-converse-stream",
        };
        f.write_str(value)
    }
}

/// Model context metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextWindow {
    pub input_tokens: u32,
    pub output_tokens: Option<u32>,
}

/// Whether a model has provider-level reasoning/thinking controls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReasoningSupport {
    None,
    Hidden,
    Exposed,
    Budgeted,
}

/// Provider-neutral model specification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSpec {
    pub model_ref: ModelRef,
    pub provider: ProviderId,
    pub api_family: ApiFamily,

    /// Actual model string sent to the provider.
    pub model_id: ModelId,

    /// Optional base URL. Required for OpenAI-compatible local/proxy providers (e.g., Azure).
    pub base_url: Option<String>,

    pub display_name: Option<String>,
    pub context_window: ContextWindow,
    pub reasoning: ReasoningSupport,
    pub pricing: Option<TokenPricing>,
}
